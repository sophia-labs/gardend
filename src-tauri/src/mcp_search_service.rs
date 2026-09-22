use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_graph_documents_cold,
    mcp_utils::{
        mcp_arg_bool, mcp_arg_string, mcp_arg_usize, mcp_query_terms, mcp_required_graph_id,
    },
    paths::existing_graph_dir,
    search_lexical_projection::push_lexical_block_hits,
    semantic_service::{semantic_search, SemanticSearchInput},
};

pub(super) fn mcp_local_search_documents(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let queries = mcp_query_terms(arguments)?;
    let mode = mcp_arg_string(arguments, &["mode"]).unwrap_or_else(|| "auto".to_string());
    let limit = mcp_arg_usize(arguments, &["limit"], 20).clamp(1, 100);
    let documents = read_graph_documents_cold(&graph_dir)?;
    let mut results = Vec::new();

    for document in documents {
        let title_lower = document.title.to_lowercase();
        let id_lower = document.document_id.to_lowercase();
        for query in &queries {
            let query_lower = query.to_lowercase();
            let matched = match mode.as_str() {
                "exact" => title_lower == query_lower || id_lower == query_lower,
                "substring" | "auto" => {
                    title_lower.contains(&query_lower) || id_lower.contains(&query_lower)
                }
                other => return Err(format!("unsupported search_documents mode: {other}")),
            };
            if matched {
                results.push(serde_json::json!({
                    "document_id": document.document_id,
                    "documentId": document.document_id,
                    "title": document.title,
                    "updated_at": document.updated_at,
                    "updatedAt": document.updated_at,
                    "block_count": document.blocks.len(),
                    "blockCount": document.blocks.len(),
                    "rdfTripleCount": document.rdf_triple_count,
                    "match": query,
                    "type": "document",
                }));
                break;
            }
        }
        if results.len() >= limit {
            break;
        }
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "query": queries.first().cloned().unwrap_or_default(),
        "queries": queries,
        "mode": mode,
        "results": results,
        "total": results.len(),
    }))
}

pub(super) fn mcp_local_search_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let queries = mcp_query_terms(arguments)?;
    let mode = mcp_arg_string(arguments, &["mode"]).unwrap_or_else(|| "hybrid".to_string());
    let limit = mcp_arg_usize(arguments, &["limit"], 30).clamp(1, 100);
    let doc_filter = mcp_arg_string(arguments, &["docFilter", "doc_filter"]);
    let case_sensitive = mcp_arg_bool(arguments, &["caseSensitive", "case_sensitive"], true);
    let documents = read_graph_documents_cold(&graph_dir)?;
    let mut results = Vec::new();
    let mut semantic_error = None;

    if mode == "lexical" || mode == "hybrid" {
        push_lexical_block_hits(
            &mut results,
            &documents,
            &queries,
            doc_filter.as_deref(),
            limit,
            case_sensitive,
        );
    }

    if (mode == "semantic" || mode == "hybrid") && results.len() < limit {
        for query in &queries {
            match semantic_search(
                app.clone(),
                SemanticSearchInput {
                    graph_id: graph_id.clone(),
                    query: query.clone(),
                    limit: Some(limit),
                },
            ) {
                Ok(search_result) => {
                    for hit in search_result.hits {
                        if let Some(doc_filter) = doc_filter.as_deref() {
                            if hit.document_id != doc_filter {
                                continue;
                            }
                        }
                        results.push(serde_json::json!({
                            "document_id": hit.document_id,
                            "documentId": hit.document_id,
                            "document_title": hit.document_title,
                            "documentTitle": hit.document_title,
                            "block_id": hit.block_id,
                            "blockId": hit.block_id,
                            "block_type": hit.block_type,
                            "blockType": hit.block_type,
                            "content": hit.content,
                            "order": hit.order,
                            "score": hit.score,
                            "match_source": "semantic",
                            "matchSource": "semantic",
                            "query": query,
                        }));
                        if results.len() >= limit {
                            break;
                        }
                    }
                }
                Err(error) if mode == "semantic" => return Err(error),
                Err(error) => semantic_error = Some(error),
            }
            if results.len() >= limit {
                break;
            }
        }
    }

    results.truncate(limit);
    Ok(serde_json::json!({
        "graph_id": graph_id,
        "query": queries.first().cloned().unwrap_or_default(),
        "queries": queries,
        "mode": mode,
        "doc_filter": doc_filter,
        "results": results,
        "total": results.len(),
        "semantic_error": semantic_error,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use serde_json::json;

    /// Regression for the all-documents hydration walks reachable from
    /// ordinary MCP traffic: `search_documents` and `get_workspace` now list
    /// documents COLD. The observable is the hydrating read's own side
    /// effect — for a legacy record whose manifest carries the inline Y.Doc
    /// update but whose sidecar file is missing, `read_document_record`
    /// BACKFILLS (writes) the sidecar as part of a mere read. A cold listing
    /// must find the document and leave the filesystem untouched. Real
    /// engine, real `document.write` records, real MCP handlers — no mocks.
    #[test]
    fn search_and_get_workspace_list_documents_cold_without_sidecar_backfill() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-search-cold-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "search-cold-walks";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Search Cold Walks".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            for (doc_id, title) in [
                ("search-doc-plain", "Plain Document"),
                ("search-doc-legacy", "Legacy Needle"),
            ] {
                crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                    app.clone(),
                    EnqueueCrdtOperationInput {
                        kind: "document.write".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(doc_id.to_string()),
                        payload: json!({
                            "documentId": doc_id,
                            "content": format!("Body of {title} with enough real text."),
                            "format": "markdown",
                            "title": title,
                        }),
                    },
                ))
                .expect("document.write drains through the real engine");
            }

            // The legacy shape: inline update kept in document.json, sidecar
            // file removed. The OLD hydrating listing would re-create this
            // file as a side effect of searching.
            let legacy_sidecar =
                crate::ydoc_paths::document_ydoc_state_path(&graph_dir, "search-doc-legacy");
            assert!(legacy_sidecar.is_file());
            std::fs::remove_file(&legacy_sidecar).expect("simulate pre-sidecar legacy record");

            let hits = mcp_local_search_documents(
                app.clone(),
                &json!({ "graphId": graph_id, "query": "needle" }),
            )
            .expect("search documents");
            assert_eq!(hits["total"], 1, "{hits}");
            assert_eq!(hits["results"][0]["document_id"], "search-doc-legacy");
            assert!(
                !legacy_sidecar.is_file(),
                "a cold search must not backfill the Y.Doc sidecar as a side effect"
            );

            let workspace = crate::app_runtime::async_runtime::block_on(
                crate::workspace_projection_service::mcp_local_get_workspace(
                    app.clone(),
                    &json!({ "graphId": graph_id }),
                ),
            )
            .expect("get workspace");
            assert!(
                workspace["counts"]["documents"].as_u64().unwrap_or(0) >= 2,
                "{workspace}"
            );
            assert!(
                !legacy_sidecar.is_file(),
                "a cold workspace projection must not backfill the Y.Doc sidecar either"
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
