use crate::rdf_service::{RdfDumpInput, RdfLoadInput, SparqlInput, SparqlUpdateInput};
use crate::sparql_admission::ExternalSparqlOptions;

fn mcp_string_arg(arguments: &serde_json::Value, name: &str) -> String {
    arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub(super) fn sparql_input_from_mcp_args(arguments: &serde_json::Value) -> SparqlInput {
    SparqlInput {
        graph_id: mcp_string_arg(arguments, "graphId"),
        query: mcp_string_arg(arguments, "query"),
    }
}

pub(super) fn sparql_update_input_from_mcp_args(
    arguments: &serde_json::Value,
) -> SparqlUpdateInput {
    SparqlUpdateInput {
        graph_id: mcp_string_arg(arguments, "graphId"),
        update: mcp_string_arg(arguments, "update"),
    }
}

pub(super) fn external_sparql_options_from_mcp_args(
    arguments: &serde_json::Value,
) -> ExternalSparqlOptions {
    ExternalSparqlOptions {
        timeout_ms: mcp_u64_arg(arguments, &["timeoutMs", "timeout_ms"]),
        max_rows: mcp_u64_arg(arguments, &["maxRows", "max_rows"])
            .and_then(|value| usize::try_from(value).ok()),
    }
}

fn mcp_u64_arg(arguments: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| arguments.get(*name).and_then(serde_json::Value::as_u64))
}

pub(super) fn rdf_load_input_from_mcp_args(arguments: &serde_json::Value) -> RdfLoadInput {
    RdfLoadInput {
        graph_id: mcp_string_arg(arguments, "graphId"),
        data: mcp_string_arg(arguments, "data"),
        format: arguments
            .get("format")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("turtle")
            .to_string(),
        base_iri: arguments
            .get("baseIri")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        target_graph_iri: arguments
            .get("targetGraphIri")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    }
}

pub(super) fn rdf_dump_input_from_mcp_args(arguments: &serde_json::Value) -> RdfDumpInput {
    RdfDumpInput {
        graph_id: mcp_string_arg(arguments, "graphId"),
        format: arguments
            .get("format")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("trig")
            .to_string(),
        source_graph_iri: arguments
            .get("sourceGraphIri")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_rdf_inputs_preserve_route_defaults() {
        let arguments = serde_json::json!({ "graph_id": "ignored" });

        let sparql = sparql_input_from_mcp_args(&arguments);
        assert_eq!(sparql.graph_id, "");
        assert_eq!(sparql.query, "");
        assert_eq!(
            external_sparql_options_from_mcp_args(&arguments).timeout_ms,
            None
        );
        assert_eq!(
            external_sparql_options_from_mcp_args(&arguments).max_rows,
            None
        );

        let update = sparql_update_input_from_mcp_args(&arguments);
        assert_eq!(update.graph_id, "");
        assert_eq!(update.update, "");

        let load = rdf_load_input_from_mcp_args(&arguments);
        assert_eq!(load.graph_id, "");
        assert_eq!(load.data, "");
        assert_eq!(load.format, "turtle");
        assert_eq!(load.base_iri, None);
        assert_eq!(load.target_graph_iri, None);

        let dump = rdf_dump_input_from_mcp_args(&arguments);
        assert_eq!(dump.graph_id, "");
        assert_eq!(dump.format, "trig");
        assert_eq!(dump.source_graph_iri, None);
    }

    #[test]
    fn mcp_rdf_inputs_preserve_camel_case_values() {
        let arguments = serde_json::json!({
            "graphId": "graph-a",
            "query": "SELECT * WHERE { ?s ?p ?o }",
            "timeoutMs": 5000,
            "maxRows": 25,
            "update": "INSERT DATA { <urn:s> <urn:p> <urn:o> }",
            "data": "<urn:s> <urn:p> <urn:o> .",
            "format": "n-triples",
            "baseIri": "https://example.test/base/",
            "targetGraphIri": "urn:graph:target",
            "sourceGraphIri": "urn:graph:source",
        });

        let sparql = sparql_input_from_mcp_args(&arguments);
        assert_eq!(sparql.graph_id, "graph-a");
        assert_eq!(sparql.query, "SELECT * WHERE { ?s ?p ?o }");
        let options = external_sparql_options_from_mcp_args(&arguments);
        assert_eq!(options.timeout_ms, Some(5000));
        assert_eq!(options.max_rows, Some(25));

        let update = sparql_update_input_from_mcp_args(&arguments);
        assert_eq!(update.graph_id, "graph-a");
        assert_eq!(update.update, "INSERT DATA { <urn:s> <urn:p> <urn:o> }");

        let load = rdf_load_input_from_mcp_args(&arguments);
        assert_eq!(load.graph_id, "graph-a");
        assert_eq!(load.data, "<urn:s> <urn:p> <urn:o> .");
        assert_eq!(load.format, "n-triples");
        assert_eq!(
            load.base_iri,
            Some("https://example.test/base/".to_string())
        );
        assert_eq!(load.target_graph_iri, Some("urn:graph:target".to_string()));

        let dump = rdf_dump_input_from_mcp_args(&arguments);
        assert_eq!(dump.graph_id, "graph-a");
        assert_eq!(dump.format, "n-triples");
        assert_eq!(dump.source_graph_iri, Some("urn:graph:source".to_string()));
    }
}
