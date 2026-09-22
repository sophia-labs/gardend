use crate::memory_semantic_recall_service::MemorySemanticRecallInput;
use crate::semantic_service::{SemanticReasonInput, SemanticSearchInput};

pub(super) fn semantic_search_input_from_mcp_args(
    arguments: &serde_json::Value,
) -> SemanticSearchInput {
    SemanticSearchInput {
        graph_id: arguments
            .get("graphId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        query: arguments
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        limit: arguments
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize),
    }
}

pub(super) fn semantic_reason_input_from_mcp_args(
    arguments: &serde_json::Value,
) -> SemanticReasonInput {
    SemanticReasonInput {
        graph_id: arguments
            .get("graphId")
            .or_else(|| arguments.get("graph_id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        head_iri: arguments
            .get("headIri")
            .or_else(|| arguments.get("head"))
            .or_else(|| arguments.get("h"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        relation_iri: arguments
            .get("relationIri")
            .or_else(|| arguments.get("relation"))
            .or_else(|| arguments.get("predicate"))
            .or_else(|| arguments.get("r"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tail_iri: arguments
            .get("tailIri")
            .or_else(|| arguments.get("tail"))
            .or_else(|| arguments.get("t"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        limit: arguments
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize),
    }
}

/// Map MCP `memory_semantic_recall` arguments (camelCase, matching the
/// `semantic_search` convention — no snake_case fallback) into
/// [`MemorySemanticRecallInput`]. Absent/non-matching-type fields default to
/// `None` (or empty string for `graphId`/`query`); the service layer is
/// responsible for turning an empty/invalid value into a loud error.
pub(super) fn memory_semantic_recall_input_from_mcp_args(
    arguments: &serde_json::Value,
) -> MemorySemanticRecallInput {
    MemorySemanticRecallInput {
        graph_id: arguments
            .get("graphId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        query: arguments
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        k: arguments
            .get("k")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize),
        cutoff: arguments
            .get("cutoff")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        include_episodes: arguments
            .get("includeEpisodes")
            .and_then(serde_json::Value::as_bool),
        min_score: arguments
            .get("minScore")
            .and_then(serde_json::Value::as_f64)
            .map(|value| value as f32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_semantic_search_input_preserves_route_defaults() {
        let input = semantic_search_input_from_mcp_args(&serde_json::json!({
            "graph_id": "ignored",
            "limit": "ignored",
        }));

        assert_eq!(input.graph_id, "");
        assert_eq!(input.query, "");
        assert_eq!(input.limit, None);
    }

    #[test]
    fn mcp_semantic_search_input_preserves_camel_case_values() {
        let input = semantic_search_input_from_mcp_args(&serde_json::json!({
            "graphId": "graph-a",
            "query": "semantic query",
            "limit": 7,
        }));

        assert_eq!(input.graph_id, "graph-a");
        assert_eq!(input.query, "semantic query");
        assert_eq!(input.limit, Some(7));
    }

    #[test]
    fn mcp_semantic_reason_input_accepts_camel_case_and_short_aliases() {
        let input = semantic_reason_input_from_mcp_args(&serde_json::json!({
            "graphId": "graph-a",
            "h": "urn:e:a",
            "relation": "supports",
            "limit": 7
        }));

        assert_eq!(input.graph_id, "graph-a");
        assert_eq!(input.head_iri.as_deref(), Some("urn:e:a"));
        assert_eq!(input.relation_iri, "supports");
        assert_eq!(input.tail_iri, None);
        assert_eq!(input.limit, Some(7));
    }

    #[test]
    fn mcp_memory_semantic_recall_input_preserves_route_defaults() {
        let input = memory_semantic_recall_input_from_mcp_args(&serde_json::json!({
            "graph_id": "ignored",
            "k": "ignored",
        }));

        assert_eq!(input.graph_id, "");
        assert_eq!(input.query, "");
        assert_eq!(input.k, None);
        assert_eq!(input.cutoff, None);
        assert_eq!(input.include_episodes, None);
        assert_eq!(input.min_score, None);
    }

    #[test]
    fn mcp_memory_semantic_recall_input_preserves_camel_case_values() {
        let input = memory_semantic_recall_input_from_mcp_args(&serde_json::json!({
            "graphId": "graph-a",
            "query": "what does vera prefer",
            "k": 5,
            "cutoff": "2026-07-01T00:00:00Z",
            "includeEpisodes": true,
            "minScore": 0.25,
        }));

        assert_eq!(input.graph_id, "graph-a");
        assert_eq!(input.query, "what does vera prefer");
        assert_eq!(input.k, Some(5));
        assert_eq!(input.cutoff.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(input.include_episodes, Some(true));
        assert_eq!(input.min_score, Some(0.25));
    }
}
