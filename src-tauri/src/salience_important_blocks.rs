use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_graph_documents_cold,
    mcp_utils::{mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    salience_score_projection::{find_projected_block, local_block_value_scores_for},
};
use std::collections::BTreeMap;

pub(super) fn mcp_local_get_important_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let document_filter = mcp_arg_string(arguments, &["document_id", "documentId"]);
    let limit = mcp_arg_usize(arguments, &["limit"], 5).clamp(1, 20);
    let valence_filter = mcp_arg_string(arguments, &["valence"]);
    let mut scored = local_block_value_scores_for(
        &app,
        &graph_id,
        &observer,
        document_filter.as_deref(),
        None,
        None,
        // Values outlive documents: a scored block whose document is gone is
        // skipped below, so rank extra candidates and truncate after the skip.
        // Otherwise `limit: 5` can return 4.
        (limit * 4).max(20),
        None,
        valence_filter.as_deref(),
    )?;
    let documents = read_graph_documents_cold(&graph_dir)?;
    let mut document_map = BTreeMap::new();
    for document in documents {
        document_map.insert(document.document_id.clone(), document);
    }

    let mut blocks = Vec::new();
    for score in scored.drain(..) {
        let document_id = score
            .get("document_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let block_id = score
            .get("block_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(document) = document_map.get(&document_id) else {
            continue;
        };
        let content = find_projected_block(document, &block_id)
            .map(|block| block.content)
            .unwrap_or_else(|| "(deleted)".to_string());
        blocks.push(serde_json::json!({
            "content": content.clone(),
            "text": content,
            "document": document.title,
            "score": score.get("composite_score").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
            "importance": score.get("cumulative_importance").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
            "valence": score.get("cumulative_valence").cloned().unwrap_or_else(|| serde_json::json!(0.0)),
            "block_id": block_id,
            "blockId": block_id,
            "doc_id": document_id,
            "docId": document_id,
            "document_id": document_id,
            "documentId": document_id,
            "block_wires": score.get("block_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "blockWires": score.get("block_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "doc_wires": score.get("doc_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "docWires": score.get("doc_wire_count").cloned().unwrap_or_else(|| serde_json::json!(0)),
            "tags": score.get("tags").cloned().unwrap_or_else(|| serde_json::json!([])),
        }));
    }
    blocks.sort_by(|left, right| {
        let left_score = left
            .get("score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let right_score = right
            .get("score")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    blocks.truncate(limit);
    Ok(serde_json::json!({
        "blocks": blocks,
        "count": blocks.len(),
        "source": "local-value-store",
        "value_store_available": true,
        "valueStoreAvailable": true,
    }))
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod tests {
    use super::mcp_local_get_important_blocks;
    use crate::app_runtime::AppHandle;
    use crate::crdt_operation_types::EnqueueCrdtOperationInput;
    use crate::crdt_queue::enqueue_crdt_operation;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::salience_mcp_valuation::mcp_local_value;
    use serde_json::json;

    const GRAPH: &str = "important-blocks-limit";

    fn value_block(app: &AppHandle, document_id: &str, block_id: &str, importance: i64) {
        mcp_local_value(
            app.clone(),
            &json!({
                "graph_id": GRAPH,
                "document_id": document_id,
                "block_id": block_id,
                "importance": importance,
            }),
        )
        .expect("value block");
    }

    #[test]
    fn important_blocks_fill_limit_when_higher_scored_documents_are_gone() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = std::env::temp_dir().join(format!(
            "garden-important-blocks-limit-{}",
            uuid::Uuid::new_v4()
        ));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(GRAPH.to_string()),
                    title: "Important Blocks".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "document.write".to_string(),
                    graph_id: GRAPH.to_string(),
                    document_id: Some("kept".to_string()),
                    payload: json!({
                        "documentId": "kept",
                        "content": "Kept paragraph.",
                        "format": "markdown",
                        "title": "Kept",
                    }),
                },
            ))
            .expect("write kept document");

            // Values outlive documents: these blocks outrank the kept ones
            // but their document does not exist.
            for index in 0..6 {
                value_block(&app, "gone", &format!("gone-{index}"), 5);
            }
            for index in 0..3 {
                value_block(&app, "kept", &format!("kept-{index}"), 1);
            }

            let response = mcp_local_get_important_blocks(
                app.clone(),
                &json!({ "graph_id": GRAPH, "limit": 3 }),
            )
            .expect("get important blocks");
            let blocks = response["blocks"].as_array().expect("blocks");
            assert_eq!(blocks.len(), 3, "{response}");
            assert!(
                blocks.iter().all(|block| block["document_id"] == "kept"),
                "{response}"
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
