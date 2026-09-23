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
    let query = json_string(hit.get("query")).unwrap_or_default();
    let mut value = serde_json::json!({
        "block_id": json_string(hit.get("block_id").or_else(|| hit.get("blockId"))).unwrap_or_default(),
        "doc_id": json_string(hit.get("document_id").or_else(|| hit.get("documentId"))).unwrap_or_default(),
        "doc_title": json_string(hit.get("document_title").or_else(|| hit.get("documentTitle"))).unwrap_or_default(),
        "text_preview": snippet_for_query(&content, &query, 180),
        "score": json_number(hit.get("score")).unwrap_or(0.0),
        "match_source": json_string(hit.get("match_source").or_else(|| hit.get("matchSource"))).unwrap_or_else(|| "lexical".to_string()),
    });
    let object = value.as_object_mut().expect("hosted hit json object");
    for key in ["lexical_score", "semantic_score"] {
        if let Some(score) = json_number(hit.get(key)) {
            object.insert(key.to_string(), serde_json::json!(score));
        }
    }
    value
}

/// A preview window CENTERED on the first occurrence of the query, so the
/// match is visible in the result list instead of whatever the block's
/// first 180 characters happen to be. Falls back to a head preview when the
/// query does not occur verbatim (semantic hits, token-only matches).
fn snippet_for_query(content: &str, query: &str, max_chars: usize) -> String {
    let compact = text_preview(content, usize::MAX);
    let query = query.trim();
    let total_chars = compact.chars().count();
    if total_chars <= max_chars || query.is_empty() {
        return text_preview(content, max_chars);
    }
    // Char-indexed case-insensitive search: byte offsets from a lowercased
    // copy cannot be used to slice the original (case folding changes byte
    // lengths for some scripts).
    let simple_lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let haystack: Vec<char> = compact.chars().map(simple_lower).collect();
    let needle: Vec<char> = query.chars().map(simple_lower).collect();
    let Some(match_char) = haystack
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
    else {
        return text_preview(content, max_chars);
    };
    let end = (match_char.saturating_sub(max_chars / 2) + max_chars).min(total_chars);
    let start = end.saturating_sub(max_chars);
    let snippet: String = compact.chars().skip(start).take(end - start).collect();
    let prefix = if start > 0 { "..." } else { "" };
    let suffix = if end < total_chars { "..." } else { "" };
    format!("{prefix}{snippet}{suffix}")
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
    fn snippet_centers_on_the_match_and_marks_elision() {
        let long_head = "filler ".repeat(40);
        let content = format!("{long_head}the settlement needle sits here and more trailing text follows it for a while");

        let centered = snippet_for_query(&content, "settlement needle", 60);
        assert!(centered.contains("settlement needle"), "{centered}");
        assert!(centered.starts_with("..."), "{centered}");

        // Query absent verbatim (semantic hit): falls back to head preview.
        let fallback = snippet_for_query(&content, "unrelated words", 60);
        assert!(fallback.starts_with("filler"), "{fallback}");

        // Short content is returned whole regardless of query.
        assert_eq!(
            snippet_for_query("short text", "text", 60),
            "short text"
        );
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
