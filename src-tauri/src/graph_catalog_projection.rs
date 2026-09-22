use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    graph_service::{list_graphs_service, GraphRecord},
    rdf::graph_subject,
    runtime_config::PROFILE_ID,
};

pub(super) fn hosted_graph_entries(
    app: AppHandle,
    include_granted_at: bool,
) -> Result<Vec<serde_json::Value>, String> {
    hosted_graph_entries_service(&app, include_granted_at).map_err(AppError::message)
}

pub(super) fn hosted_graph_entries_service(
    app: &AppHandle,
    include_granted_at: bool,
) -> AppResult<Vec<serde_json::Value>> {
    Ok(list_graphs_service(app)?
        .into_iter()
        .map(|graph| hosted_graph_entry_from_record(graph, include_granted_at))
        .collect())
}

pub(super) fn mcp_local_list_graphs(app: AppHandle) -> Result<serde_json::Value, String> {
    Ok(mcp_list_graphs_response(hosted_graph_entries(app, true)?))
}

fn mcp_list_graphs_response(graphs: Vec<serde_json::Value>) -> serde_json::Value {
    let count = graphs.len();
    serde_json::json!({
        "graphs": graphs,
        "count": count,
    })
}

pub(super) fn hosted_graph_entry_from_record(
    graph: GraphRecord,
    include_granted_at: bool,
) -> serde_json::Value {
    let created_at = graph.created_at.clone();
    let mut entry = serde_json::json!({
        "graph_uri": graph_subject(&graph.graph_id),
        "graph_id": graph.graph_id,
        "title": graph.title,
        "description": graph.description,
        "status": graph.status,
        "created_at": created_at,
        "updated_at": graph.updated_at,
        "triple_count": null,
        "last_query_at": null,
        "last_update_at": null,
        "role": "owner",
        "owner_user_id": PROFILE_ID,
        "graph_incarnation": graph.incarnation_id.clone(),
        "graphIncarnation": graph.incarnation_id,
        "created_by_operation_id": graph.created_by_operation_id.clone(),
        "createdByOperationId": graph.created_by_operation_id,
    });
    if include_granted_at {
        if let Some(object) = entry.as_object_mut() {
            object.insert("granted_at".to_string(), serde_json::json!(created_at));
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_config::{LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};

    fn graph_record() -> GraphRecord {
        GraphRecord {
            graph_id: "graph-a".to_string(),
            title: "Graph A".to_string(),
            description: Some("Description".to_string()),
            status: "active".to_string(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: "/tmp/graph-a".to_string(),
            created_at: "1000".to_string(),
            incarnation_id: None,
            updated_at: "2000".to_string(),
            capabilities: Vec::new(),
            created_by_operation_id: None,
            validation_policy: crate::runtime_config::ValidationPolicy::default(),
            content_revision: None,
        }
    }

    #[test]
    fn hosted_graph_entry_preserves_local_catalog_shape() {
        let without_grant = hosted_graph_entry_from_record(graph_record(), false);
        assert_eq!(without_grant["graph_uri"], graph_subject("graph-a"));
        assert_eq!(without_grant["graph_id"], "graph-a");
        assert_eq!(without_grant["title"], "Graph A");
        assert_eq!(without_grant["description"], "Description");
        assert_eq!(without_grant["status"], "active");
        assert_eq!(without_grant["role"], "owner");
        assert_eq!(without_grant["owner_user_id"], PROFILE_ID);
        assert!(without_grant.get("granted_at").is_none());

        let with_grant = hosted_graph_entry_from_record(graph_record(), true);
        assert_eq!(with_grant["granted_at"], "1000");
    }

    #[test]
    fn mcp_list_graphs_response_is_a_structured_content_object() {
        let response =
            mcp_list_graphs_response(vec![hosted_graph_entry_from_record(graph_record(), true)]);
        assert!(response.is_object());
        assert_eq!(response["count"], 1);
        assert_eq!(response["graphs"][0]["graph_id"], "graph-a");
    }
}
