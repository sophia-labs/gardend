use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FencedCreateGraphInput {
    pub(super) title: String,
    #[serde(default, alias = "graph_id")]
    pub(super) graph_id: Option<String>,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default, alias = "operation_id")]
    pub(super) operation_id: Option<String>,
    #[serde(default, alias = "graph_incarnation")]
    pub(super) graph_incarnation: Option<String>,
}

impl FencedCreateGraphInput {
    pub(super) fn into_parts(self) -> (crate::graph_service::CreateGraphInput, Option<String>) {
        (
            crate::graph_service::CreateGraphInput {
                title: self.title,
                graph_id: self.graph_id,
                description: self.description,
                operation_id: self.operation_id,
            },
            self.graph_incarnation,
        )
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct LocalGraphQueryRequest {
    #[serde(default)]
    sparql: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    update: Option<String>,
    #[serde(default, alias = "graphId")]
    graph_id: Option<String>,
    #[serde(default, alias = "resultFormat")]
    pub(super) result_format: Option<String>,
    #[serde(default, alias = "timeoutMs")]
    pub(super) timeout_ms: Option<u64>,
    #[serde(default, alias = "maxRows")]
    pub(super) max_rows: Option<usize>,
}

impl LocalGraphQueryRequest {
    pub(super) fn sparql(&self) -> Result<String, String> {
        let value = self
            .sparql
            .as_deref()
            .or(self.query.as_deref())
            .or(self.update.as_deref())
            .unwrap_or_default()
            .trim();
        if value.is_empty() {
            return Err("SPARQL query/update is required".to_string());
        }
        Ok(value.to_string())
    }

    pub(super) fn graph_id(&self) -> Result<String, String> {
        let graph_id = self.graph_id.as_deref().unwrap_or_default().trim();
        if graph_id.is_empty() {
            return Err("graph_id is required for local graph jobs".to_string());
        }
        Ok(graph_id.to_string())
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct DeleteGraphQuery {
    #[serde(default)]
    pub(super) hard: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphExportInput {
    #[serde(default, alias = "include_artifacts")]
    pub(super) include_artifacts: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphDuplicateInput {
    #[serde(alias = "new_graph_id")]
    pub(super) new_graph_id: String,
    #[serde(default, alias = "new_title")]
    pub(super) new_title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentPreflightQuery {
    #[serde(default)]
    pub(super) simulate: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct HostedGraphListQuery {
    #[serde(alias = "wait_ms")]
    pub(super) wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HostedWorkspaceVizQuery {
    #[serde(default, alias = "wait_ms")]
    pub(super) wait_ms: Option<u64>,
    #[serde(default, alias = "limit_nodes")]
    pub(super) limit_nodes: Option<usize>,
    #[serde(default, alias = "limit_edges")]
    pub(super) limit_edges: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_graph_query_request_accepts_sparql_or_query_aliases() {
        let sparql = serde_json::from_value::<LocalGraphQueryRequest>(serde_json::json!({
            "graphId": " graph-a ",
            "sparql": " SELECT * WHERE { ?s ?p ?o } ",
            "resultFormat": "json",
            "timeoutMs": 100,
            "maxRows": 25,
        }))
        .unwrap();
        assert_eq!(sparql.graph_id().unwrap(), "graph-a");
        assert_eq!(sparql.sparql().unwrap(), "SELECT * WHERE { ?s ?p ?o }");
        assert_eq!(sparql.result_format.as_deref(), Some("json"));
        assert_eq!(sparql.timeout_ms, Some(100));
        assert_eq!(sparql.max_rows, Some(25));

        let query = serde_json::from_value::<LocalGraphQueryRequest>(serde_json::json!({
            "graph_id": "graph-b",
            "query": "ASK { ?s ?p ?o }",
        }))
        .unwrap();
        assert_eq!(query.graph_id().unwrap(), "graph-b");
        assert_eq!(query.sparql().unwrap(), "ASK { ?s ?p ?o }");

        let update = serde_json::from_value::<LocalGraphQueryRequest>(serde_json::json!({
            "graph_id": "graph-c",
            "update": "INSERT DATA { <urn:s> <urn:p> <urn:o> }",
        }))
        .unwrap();
        assert_eq!(update.graph_id().unwrap(), "graph-c");
        assert_eq!(
            update.sparql().unwrap(),
            "INSERT DATA { <urn:s> <urn:p> <urn:o> }"
        );
    }

    #[test]
    fn local_graph_query_request_reports_missing_required_fields() {
        let request =
            serde_json::from_value::<LocalGraphQueryRequest>(serde_json::json!({})).unwrap();

        assert_eq!(
            request.graph_id().unwrap_err(),
            "graph_id is required for local graph jobs"
        );
        assert_eq!(
            request.sparql().unwrap_err(),
            "SPARQL query/update is required"
        );
    }

    #[test]
    fn graph_input_aliases_preserve_hosted_and_local_shapes() {
        let duplicate = serde_json::from_value::<GraphDuplicateInput>(serde_json::json!({
            "new_graph_id": "copy-a",
            "new_title": "Copy A",
        }))
        .unwrap();
        assert_eq!(duplicate.new_graph_id, "copy-a");
        assert_eq!(duplicate.new_title.as_deref(), Some("Copy A"));

        let export = serde_json::from_value::<GraphExportInput>(serde_json::json!({
            "include_artifacts": true,
        }))
        .unwrap();
        assert!(export.include_artifacts);

        let viz = serde_json::from_value::<HostedWorkspaceVizQuery>(serde_json::json!({
            "wait_ms": 10,
            "limit_nodes": 20,
            "limit_edges": 30,
        }))
        .unwrap();
        assert_eq!(viz.wait_ms, Some(10));
        assert_eq!(viz.limit_nodes, Some(20));
        assert_eq!(viz.limit_edges, Some(30));
    }
}
