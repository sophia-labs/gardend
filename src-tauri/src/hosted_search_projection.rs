use crate::app_runtime::AppHandle;
use crate::{
    json_utils::{json_number, json_string},
    mcp_search_service::mcp_local_search_blocks,
    mcp_utils::{mcp_arg_string, mcp_required_graph_id},
    semantic_service::{semantic_search, SemanticSearchInput},
    text_utils::text_preview,
};

fn search_body_argument<'a>(
    arguments: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|key| arguments.get(*key))
}

fn search_body_usize(arguments: &serde_json::Value, keys: &[&str], default: usize) -> usize {
    search_body_argument(arguments, keys)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

fn search_body_f64(arguments: &serde_json::Value, keys: &[&str], default: f64) -> f64 {
    search_body_argument(arguments, keys)
        .and_then(|value| json_number(Some(value)))
        .unwrap_or(default)
}

fn hosted_search_hit_from_local(hit: &serde_json::Value) -> serde_json::Value {
    let content = json_string(hit.get("content")).unwrap_or_default();
    serde_json::json!({
        "block_id": json_string(hit.get("block_id").or_else(|| hit.get("blockId"))).unwrap_or_default(),
        "doc_id": json_string(hit.get("document_id").or_else(|| hit.get("documentId"))).unwrap_or_default(),
        "doc_title": json_string(hit.get("document_title").or_else(|| hit.get("documentTitle"))).unwrap_or_default(),
        "text_preview": text_preview(&content, 180),
        "score": json_number(hit.get("score")).unwrap_or(0.0),
        "match_source": json_string(hit.get("match_source").or_else(|| hit.get("matchSource"))).unwrap_or_else(|| "lexical".to_string()),
    })
}

pub(super) fn hosted_block_search_response(
    app: AppHandle,
    input: &serde_json::Value,
    mode: &str,
) -> Result<serde_json::Value, String> {
    let mut arguments = input.clone();
    let Some(object) = arguments.as_object_mut() else {
        return Err("search request body must be an object".to_string());
    };
    object.insert("mode".to_string(), serde_json::json!(mode));
    let local = mcp_local_search_blocks(app, &arguments)?;
    let results = local
        .get("results")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(hosted_search_hit_from_local)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let lexical_count = results
        .iter()
        .filter(|hit| {
            json_string(hit.get("match_source")).as_deref() == Some("lexical")
                || json_string(hit.get("match_source")).as_deref() == Some("both")
        })
        .count();
    let semantic_count = results
        .iter()
        .filter(|hit| {
            json_string(hit.get("match_source")).as_deref() == Some("semantic")
                || json_string(hit.get("match_source")).as_deref() == Some("both")
        })
        .count();

    Ok(serde_json::json!({
        "query": json_string(local.get("query")).unwrap_or_default(),
        "results": results,
        "count": results.len(),
        "lexical_count": lexical_count,
        "semantic_count": semantic_count,
    }))
}

pub(super) fn hosted_semantic_search_response(
    app: AppHandle,
    input: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(input)?;
    let query = mcp_arg_string(input, &["query"]).ok_or_else(|| "query is required".to_string())?;
    let limit = search_body_usize(input, &["limit"], 10).clamp(1, 100);
    let min_score = search_body_f64(input, &["minScore", "min_score"], 0.0);
    let doc_filter = mcp_arg_string(input, &["docFilter", "doc_filter"]);
    let result = semantic_search(
        app,
        SemanticSearchInput {
            graph_id,
            query: query.clone(),
            limit: Some(limit),
        },
    )?;
    let hits = result
        .hits
        .into_iter()
        .filter(|hit| hit.score as f64 >= min_score)
        .filter(|hit| {
            doc_filter
                .as_deref()
                .map(|document_id| hit.document_id == document_id)
                .unwrap_or(true)
        })
        .map(|hit| {
            serde_json::json!({
                "block_id": hit.block_id,
                "doc_id": hit.document_id,
                "doc_title": hit.document_title,
                "text_preview": text_preview(&hit.content, 180),
                "score": hit.score,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "query": query,
        "results": hits,
        "total_count": hits.len(),
        "model": result.model_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_body_scalars_accept_hosted_aliases_and_defaults() {
        let body = serde_json::json!({
            "limit": 25,
            "min_score": "0.72",
        });

        assert_eq!(search_body_usize(&body, &["limit"], 10), 25);
        assert_eq!(search_body_usize(&body, &["missing"], 10), 10);
        assert_eq!(
            search_body_f64(&body, &["minScore", "min_score"], 0.0),
            0.72
        );
        assert_eq!(search_body_f64(&body, &["missing"], 0.5), 0.5);
        assert!(search_body_argument(&body, &["missing", "limit"]).is_some());
    }

    #[test]
    fn hosted_search_hit_normalizes_local_aliases() {
        let hit = hosted_search_hit_from_local(&serde_json::json!({
            "blockId": "block-a",
            "documentId": "doc-a",
            "documentTitle": "Document A",
            "content": "  alpha\n beta   gamma ",
            "score": "0.5",
            "matchSource": "semantic"
        }));

        assert_eq!(hit["block_id"], "block-a");
        assert_eq!(hit["doc_id"], "doc-a");
        assert_eq!(hit["doc_title"], "Document A");
        assert_eq!(hit["text_preview"], "alpha beta gamma");
        assert_eq!(hit["score"], 0.5);
        assert_eq!(hit["match_source"], "semantic");
    }

    #[test]
    fn hosted_search_hit_defaults_missing_values() {
        let hit = hosted_search_hit_from_local(&serde_json::json!({}));

        assert_eq!(hit["block_id"], "");
        assert_eq!(hit["doc_id"], "");
        assert_eq!(hit["doc_title"], "");
        assert_eq!(hit["text_preview"], "");
        assert_eq!(hit["score"], 0.0);
        assert_eq!(hit["match_source"], "lexical");
    }
}
