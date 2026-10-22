use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_workspace_record,
    json_utils::json_string,
    mcp_utils::{mcp_arg_string, mcp_arg_usize, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    rdf_service::wire_predicate_uri,
    wire_state::wire_is_active,
    wire_traversal_payloads::{
        document_title_for_traversal, iri_titles_of, is_iri_node, traversal_node_entry,
        wire_traversal_hop, wire_traversal_targets,
    },
    workspace_entity_projection::{workspace_documents, workspace_entity_id, workspace_wires},
};
use std::collections::{BTreeMap, VecDeque};

pub(super) fn mcp_local_traverse_wires(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    // The walk starts at a document or at an object: exactly one of `document_id` and `iri`.
    let start_document_id = mcp_arg_string(arguments, &["document_id", "documentId"]);
    let start_iri = mcp_arg_string(arguments, &["iri"]);
    let document_id = match (&start_document_id, &start_iri) {
        (Some(document_id), None) => document_id.clone(),
        (None, Some(iri)) => iri.clone(),
        (None, None) => return Err("document_id is required (or give an iri)".to_string()),
        (Some(_), Some(_)) => {
            return Err("traverse_wires: give exactly one of document_id and iri".to_string())
        }
    };
    let max_depth = mcp_arg_usize(arguments, &["max_depth", "maxDepth", "depth"], 2).min(6);
    let direction = mcp_arg_string(arguments, &["direction"]).unwrap_or_else(|| "both".to_string());
    if !matches!(direction.as_str(), "outgoing" | "incoming" | "both") {
        return Err("direction must be outgoing, incoming, or both".to_string());
    }
    let predicate_filter =
        mcp_arg_string(arguments, &["predicate"]).map(|value| wire_predicate_uri(&value));

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let snapshot = workspace.snapshot.as_ref();
    let documents = workspace_documents(snapshot);
    let documents_by_id = documents
        .into_iter()
        .filter_map(|document| workspace_entity_id(&document).map(|id| (id, document)))
        .collect::<BTreeMap<_, _>>();
    let wires = workspace_wires(snapshot)
        .into_iter()
        .filter(|wire| {
            let id = json_string(wire.get("id")).unwrap_or_default();
            if id.ends_with("-inv") || !wire_is_active(wire) {
                return false;
            }
            if let Some(predicate_filter) = &predicate_filter {
                let predicate =
                    json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
                return wire_predicate_uri(&predicate) == *predicate_filter;
            }
            true
        })
        .collect::<Vec<_>>();

    let iri_titles = iri_titles_of(&wires);
    let mut visited_depths = BTreeMap::new();
    let mut queue = VecDeque::new();
    let mut paths = Vec::new();
    visited_depths.insert(document_id.clone(), 0usize);
    queue.push_back((document_id.clone(), 0usize, Vec::<serde_json::Value>::new()));

    while let Some((current_document_id, depth, path)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        for wire in &wires {
            for next_document_id in wire_traversal_targets(wire, &current_document_id, &direction) {
                if next_document_id == current_document_id
                    || visited_depths.contains_key(&next_document_id)
                {
                    continue;
                }
                let next_depth = depth + 1;
                let mut next_path = path.clone();
                next_path.push(wire_traversal_hop(
                    wire,
                    &current_document_id,
                    &next_document_id,
                ));
                visited_depths.insert(next_document_id.clone(), next_depth);
                paths.push(serde_json::json!({
                    "document_id": if is_iri_node(&next_document_id) { None } else { Some(next_document_id.clone()) },
                    "documentId": if is_iri_node(&next_document_id) { None } else { Some(next_document_id.clone()) },
                    "title": if is_iri_node(&next_document_id) {
                        iri_titles.get(&next_document_id).cloned().unwrap_or_else(|| next_document_id.clone())
                    } else {
                        document_title_for_traversal(&documents_by_id, &next_document_id).unwrap_or_else(|| next_document_id.clone())
                    },
                    "depth": next_depth,
                    "hops": next_path,
                }));
                if is_iri_node(&next_document_id) {
                    if let Some(entry) = paths.last_mut() {
                        entry["iri"] = serde_json::json!(next_document_id.clone());
                    }
                }
                queue.push_back((next_document_id, next_depth, next_path));
            }
        }
    }

    let mut reachable_documents = visited_depths
        .iter()
        .map(|(node, depth)| traversal_node_entry(&documents_by_id, &iri_titles, node, *depth))
        .collect::<Vec<_>>();
    reachable_documents.sort_by(|left, right| {
        let left_depth = left
            .get("depth")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        let right_depth = right
            .get("depth")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        let key = |entry: &serde_json::Value| {
            json_string(entry.get("documentId")).or_else(|| json_string(entry.get("iri")))
        };
        left_depth.cmp(&right_depth).then_with(|| key(left).cmp(&key(right)))
    });
    let reachable_count = reachable_documents.len().saturating_sub(1);
    let document_count = reachable_documents.len();
    let path_count = paths.len();

    let mut response = serde_json::json!({
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "document_id": start_document_id.clone(),
        "documentId": start_document_id,
        "direction": direction,
        "max_depth": max_depth,
        "maxDepth": max_depth,
        "predicate": predicate_filter,
        "documents": reachable_documents,
        "paths": paths,
        "count": document_count,
        "reachable_count": reachable_count,
        "path_count": path_count,
    });
    if let Some(iri) = start_iri {
        response["iri"] = serde_json::json!(iri);
    }
    Ok(response)
}
