use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp, document_service::read_graph_documents_cold, graph_service::GraphRecord,
    json_utils::json_string, mcp_utils::mcp_graph_id_or_default, paths::existing_graph_dir,
    profile_service::ProfileManifest, runtime_config::LOCAL_PROVIDER_ID,
};
use std::collections::BTreeMap;

pub(super) fn mcp_local_orientation_location(
    profile: &ProfileManifest,
    graph: &GraphRecord,
) -> serde_json::Value {
    serde_json::json!({
        "graph_id": graph.graph_id.clone(),
        "graphId": graph.graph_id.clone(),
        "profile_id": profile.profile_id.clone(),
        "profileId": profile.profile_id.clone(),
        "runtime_profile": profile.runtime_profile.clone(),
        "runtimeProfile": profile.runtime_profile.clone(),
        "provider_id": LOCAL_PROVIDER_ID,
        "providerId": LOCAL_PROVIDER_ID,
        "timestamp": timestamp(),
        "graph": {
            "graph_id": graph.graph_id.clone(),
            "graphId": graph.graph_id.clone(),
            "title": graph.title.clone(),
            "description": graph.description.clone(),
            "updated_at": graph.updated_at.clone(),
            "updatedAt": graph.updated_at.clone(),
        },
    })
}

pub(super) fn mcp_local_surface(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let actions = arguments
        .get("actions")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if actions.is_empty() {
        return Ok(serde_json::json!({
            "type": "surface",
            "actions": [],
        }));
    }

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let titles = read_graph_documents_cold(&graph_dir)?
        .into_iter()
        .map(|document| (document.document_id, document.title))
        .collect::<BTreeMap<_, _>>();
    let resolved = actions
        .iter()
        .filter_map(|action| action.as_object())
        .map(|action| surface_action_entry(action, &titles))
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "type": "surface",
        "actions": resolved,
    }))
}

fn surface_action_entry(
    action: &serde_json::Map<String, serde_json::Value>,
    titles: &BTreeMap<String, String>,
) -> serde_json::Value {
    let document_id = json_string(
        action
            .get("document_id")
            .or_else(|| action.get("documentId")),
    )
    .unwrap_or_default();
    let title = titles
        .get(&document_id)
        .cloned()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| document_id.clone());
    let mut entry = serde_json::json!({
        "document_id": document_id.clone(),
        "documentId": document_id,
        "title": title,
        "action": json_string(action.get("action")).unwrap_or_default(),
    });
    if let Some(block_id) = json_string(action.get("block_id").or_else(|| action.get("blockId"))) {
        entry["block_id"] = serde_json::json!(block_id);
        entry["blockId"] = entry["block_id"].clone();
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_config::{GRAPH_STATUS_ACTIVE, LOCAL_GRAPH_ORIGIN};

    #[test]
    fn orientation_location_preserves_hosted_and_local_aliases() {
        let profile = ProfileManifest {
            profile_id: "default".to_string(),
            display_name: "Default".to_string(),
            runtime_profile: "local_only".to_string(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
        };
        let graph = GraphRecord {
            graph_id: "graph-one".to_string(),
            title: "Graph One".to_string(),
            description: Some("desc".to_string()),
            status: GRAPH_STATUS_ACTIVE.to_string(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: "/tmp/graph-one".to_string(),
            created_at: "1".to_string(),
            incarnation_id: None,
            updated_at: "3".to_string(),
            capabilities: vec![],
            created_by_operation_id: None,
            validation_policy: crate::runtime_config::ValidationPolicy::default(),
            content_revision: None,
        };

        let location = mcp_local_orientation_location(&profile, &graph);

        assert_eq!(location["graph_id"], "graph-one");
        assert_eq!(location["graphId"], "graph-one");
        assert_eq!(location["profile_id"], "default");
        assert_eq!(location["providerId"], LOCAL_PROVIDER_ID);
        assert_eq!(location["graph"]["title"], "Graph One");
        assert!(location.get("timestamp").is_some());
    }

    #[test]
    fn surface_action_entry_resolves_titles_and_block_aliases() {
        let action = serde_json::json!({
            "documentId": "doc-one",
            "block_id": "block-one",
            "action": "read"
        });
        let titles = BTreeMap::from([("doc-one".to_string(), "Document One".to_string())]);
        let entry = surface_action_entry(action.as_object().unwrap(), &titles);

        assert_eq!(entry["document_id"], "doc-one");
        assert_eq!(entry["documentId"], "doc-one");
        assert_eq!(entry["title"], "Document One");
        assert_eq!(entry["block_id"], "block-one");
        assert_eq!(entry["blockId"], "block-one");
        assert_eq!(entry["action"], "read");
    }

    #[test]
    fn surface_action_entry_falls_back_to_document_id_title() {
        let action = serde_json::json!({
            "document_id": "missing-doc",
            "action": "open"
        });
        let entry = surface_action_entry(action.as_object().unwrap(), &BTreeMap::new());

        assert_eq!(entry["title"], "missing-doc");
        assert!(entry.get("block_id").is_none());
    }
}
