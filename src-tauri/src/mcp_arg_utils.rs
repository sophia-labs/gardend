pub(super) fn mcp_arg_string(arguments: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        arguments
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

pub(super) fn mcp_arg_string_vec(arguments: &serde_json::Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|key| arguments.get(*key))
        .map(|value| match value {
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>(),
            serde_json::Value::String(value) if !value.trim().is_empty() => {
                vec![value.trim().to_string()]
            }
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

pub(super) fn mcp_arg_usize(arguments: &serde_json::Value, keys: &[&str], default: usize) -> usize {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(serde_json::Value::as_u64))
        .map(|value| value as usize)
        .unwrap_or(default)
}

pub(super) fn mcp_arg_u64(arguments: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(serde_json::Value::as_u64))
}

pub(super) fn mcp_arg_u64_vec(arguments: &serde_json::Value, keys: &[&str]) -> Vec<u64> {
    keys.iter()
        .find_map(|key| arguments.get(*key))
        .map(|value| match value {
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .collect::<Vec<_>>(),
            serde_json::Value::Number(number) => number.as_u64().into_iter().collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

pub(super) fn mcp_arg_isize(arguments: &serde_json::Value, keys: &[&str], default: isize) -> isize {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(serde_json::Value::as_i64))
        .map(|value| value as isize)
        .unwrap_or(default)
}

pub(super) fn mcp_arg_bool(arguments: &serde_json::Value, keys: &[&str], default: bool) -> bool {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(serde_json::Value::as_bool))
        .unwrap_or(default)
}

pub(super) fn mcp_query_terms(arguments: &serde_json::Value) -> Result<Vec<String>, String> {
    if let Some(values) = arguments
        .get("queries")
        .and_then(serde_json::Value::as_array)
    {
        let terms = values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !terms.is_empty() {
            return Ok(terms);
        }
    }
    mcp_arg_string(arguments, &["query"])
        .map(|query| vec![query])
        .ok_or_else(|| "query or queries is required".to_string())
}

pub(super) fn mcp_required_graph_id(arguments: &serde_json::Value) -> Result<String, String> {
    mcp_arg_string(arguments, &["graphId", "graph_id"])
        .ok_or_else(|| "graphId is required".to_string())
}

pub(super) fn mcp_required_document_id(arguments: &serde_json::Value) -> Result<String, String> {
    mcp_arg_string(arguments, &["documentId", "document_id"])
        .ok_or_else(|| "documentId is required".to_string())
}

pub(super) fn mcp_required_job_id(arguments: &serde_json::Value) -> Result<String, String> {
    mcp_arg_string(arguments, &["job_id", "jobId"]).ok_or_else(|| "job_id is required".to_string())
}

pub(super) fn mcp_block_target(arguments: &serde_json::Value) -> Result<(String, String), String> {
    Ok((
        mcp_required_graph_id(arguments)?,
        mcp_required_document_id(arguments)?,
    ))
}

pub(super) fn mcp_arg_string_with_fallback(
    primary: &serde_json::Value,
    fallback: &serde_json::Value,
    keys: &[&str],
) -> Option<String> {
    mcp_arg_string(primary, keys).or_else(|| mcp_arg_string(fallback, keys))
}

pub(super) fn mcp_arg_bool_with_fallback(
    primary: &serde_json::Value,
    fallback: &serde_json::Value,
    keys: &[&str],
    default: bool,
) -> bool {
    keys.iter()
        .find_map(|key| {
            primary
                .get(*key)
                .or_else(|| fallback.get(*key))
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_arg_helpers_trim_aliases_and_preserve_defaults() {
        let arguments = serde_json::json!({
            "graph_id": " graph-a ",
            "documentIds": ["doc-a", " ", "doc-b"],
            "limit": 25,
            "offset": -4,
            "includeIds": false,
            "numbers": [1, 2, "bad"],
            "number": 7,
        });

        assert_eq!(
            mcp_arg_string(&arguments, &["graphId", "graph_id"]),
            Some("graph-a".to_string())
        );
        assert_eq!(
            mcp_arg_string_vec(&arguments, &["document_ids", "documentIds"]),
            vec!["doc-a".to_string(), "doc-b".to_string()]
        );
        assert_eq!(mcp_arg_usize(&arguments, &["limit"], 5), 25);
        assert_eq!(mcp_arg_isize(&arguments, &["offset"], 0), -4);
        assert!(!mcp_arg_bool(&arguments, &["includeIds"], true));
        assert_eq!(mcp_arg_u64(&arguments, &["number"]), Some(7));
        assert_eq!(mcp_arg_u64_vec(&arguments, &["numbers"]), vec![1, 2]);
        assert_eq!(mcp_arg_usize(&arguments, &["missing"], 9), 9);
    }

    #[test]
    fn mcp_query_terms_accepts_batch_or_single_query() {
        assert_eq!(
            mcp_query_terms(&serde_json::json!({ "queries": [" a ", "", "b"] })).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            mcp_query_terms(&serde_json::json!({ "query": " one " })).unwrap(),
            vec!["one".to_string()]
        );
        assert!(mcp_query_terms(&serde_json::json!({ "queries": [] })).is_err());
    }

    #[test]
    fn mcp_required_ids_report_contract_errors() {
        let arguments = serde_json::json!({
            "graphId": "graph-a",
            "document_id": "doc-a",
            "jobId": "job-a"
        });

        assert_eq!(mcp_required_graph_id(&arguments).unwrap(), "graph-a");
        assert_eq!(mcp_required_document_id(&arguments).unwrap(), "doc-a");
        assert_eq!(mcp_required_job_id(&arguments).unwrap(), "job-a");
        assert!(mcp_required_graph_id(&serde_json::json!({})).is_err());
        assert!(mcp_required_document_id(&serde_json::json!({})).is_err());
        assert!(mcp_required_job_id(&serde_json::json!({})).is_err());
    }

    #[test]
    fn fallback_helpers_prefer_primary_values() {
        let primary = serde_json::json!({ "predicate": "primary", "bidirectional": true });
        let fallback = serde_json::json!({ "predicate": "fallback", "bidirectional": false });

        assert_eq!(
            mcp_arg_string_with_fallback(&primary, &fallback, &["predicate"]),
            Some("primary".to_string())
        );
        assert!(mcp_arg_bool_with_fallback(
            &primary,
            &fallback,
            &["bidirectional"],
            false
        ));
        assert_eq!(
            mcp_arg_string_with_fallback(&serde_json::json!({}), &fallback, &["predicate"]),
            Some("fallback".to_string())
        );
    }
}
