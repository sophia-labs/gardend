use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_workspace_record,
    json_utils::{json_bool, json_string},
    mcp_utils::{mcp_arg_string, mcp_required_document_id, mcp_required_graph_id},
    paths::existing_graph_dir,
    rdf_service::wire_predicate_uri,
    wire_predicates::{builtin_wire_predicates, predicate_label, predicate_short_name},
    wire_state::{wire_is_active, wire_matches_document},
    workspace_entity_projection::{workspace_document_title, workspace_documents, workspace_wires},
};
use std::collections::BTreeSet;

fn wire_summary(
    wire: &serde_json::Value,
    graph_id: &str,
    documents: &[serde_json::Value],
) -> serde_json::Value {
    let predicate = json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
    let predicate_uri = wire_predicate_uri(&predicate);
    let source_document_id = json_string(wire.get("sourceDocumentId"));
    let target_document_id = json_string(wire.get("targetDocumentId"));
    let source_title = json_string(wire.get("sourceTitle"))
        .or_else(|| workspace_document_title(documents, source_document_id.as_deref()));
    let target_title = json_string(wire.get("targetTitle"))
        .or_else(|| workspace_document_title(documents, target_document_id.as_deref()));

    let mut value = serde_json::json!({
        "id": json_string(wire.get("id")).unwrap_or_default(),
        "sourceGraphId": graph_id,
        "sourceDocumentId": source_document_id,
        "targetGraphId": json_string(wire.get("targetGraphId")).unwrap_or_else(|| graph_id.to_string()),
        "targetDocumentId": target_document_id,
        "predicate": predicate_short_name(&predicate_uri),
        "predicateUri": predicate_uri,
        "predicateLabel": predicate_label(&predicate),
        "bidirectional": json_bool(wire.get("bidirectional")).unwrap_or(false),
    });

    for (json_key, output_key) in [
        ("sourceBlockId", "sourceBlockId"),
        ("targetBlockId", "targetBlockId"),
        ("sourceMarkId", "sourceMarkId"),
        ("targetMarkId", "targetMarkId"),
        ("inverseOf", "inverseOf"),
        ("sourceSnippet", "sourceSnippet"),
        ("targetSnippet", "targetSnippet"),
    ] {
        if let Some(field_value) = json_string(wire.get(json_key)) {
            value[output_key] = serde_json::json!(field_value);
        }
    }
    if let Some(title) = source_title {
        value["sourceTitle"] = serde_json::json!(title);
    }
    if let Some(title) = target_title {
        value["targetTitle"] = serde_json::json!(title);
    }
    value
}

pub(super) fn mcp_local_get_wires(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let direction = mcp_arg_string(arguments, &["direction"]).unwrap_or_else(|| "both".to_string());
    if !matches!(direction.as_str(), "outgoing" | "incoming" | "both") {
        return Err("direction must be outgoing, incoming, or both".to_string());
    }
    let predicate =
        mcp_arg_string(arguments, &["predicate"]).map(|value| wire_predicate_uri(&value));
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let snapshot = workspace.snapshot.as_ref();
    let documents = workspace_documents(snapshot);
    let wires = workspace_wires(snapshot);
    let mut results = Vec::new();

    for wire in wires {
        let id = json_string(wire.get("id")).unwrap_or_default();
        if id.ends_with("-inv") || !wire_is_active(&wire) {
            continue;
        }
        if !wire_matches_document(&wire, &document_id, &direction) {
            continue;
        }
        if let Some(predicate) = &predicate {
            let wire_predicate =
                json_string(wire.get("predicate")).unwrap_or_else(|| "relatedTo".to_string());
            if wire_predicate_uri(&wire_predicate) != *predicate {
                continue;
            }
        }
        results.push(wire_summary(&wire, &graph_id, &documents));
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "document_id": document_id,
        "direction": direction,
        "wires": results,
        "count": results.len(),
    }))
}

pub(super) fn mcp_local_list_wire_predicates(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let mut predicates = Vec::new();
    let mut seen = BTreeSet::new();

    for (name, label, category) in builtin_wire_predicates() {
        seen.insert(name.to_string());
        predicates.push(serde_json::json!({
            "name": name,
            "label": label,
            "category": if category.is_empty() { serde_json::Value::Null } else { serde_json::json!(category) },
        }));
    }

    for wire in workspace_wires(workspace.snapshot.as_ref()) {
        let Some(predicate) = json_string(wire.get("predicate")) else {
            continue;
        };
        let short_name = predicate_short_name(&predicate);
        if seen.insert(short_name.clone()) {
            predicates.push(serde_json::json!({
                "name": short_name,
                "label": predicate_label(&predicate),
                "category": "Custom",
            }));
        }
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "predicates": predicates,
        "count": predicates.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_summary_normalizes_predicates_and_document_titles() {
        let wire = serde_json::json!({
            "id": "wire-a",
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
            "predicate": "http://mnemosyne.ai/vocab#supports",
            "sourceBlockId": "block-a",
            "targetSnippet": "target text",
            "bidirectional": true
        });
        let documents = vec![
            serde_json::json!({ "documentId": "doc-a", "title": "Source" }),
            serde_json::json!({ "documentId": "doc-b", "title": "Target" }),
        ];

        let value = wire_summary(&wire, "graph-a", &documents);

        assert_eq!(value["id"], "wire-a");
        assert_eq!(value["sourceGraphId"], "graph-a");
        assert_eq!(value["targetGraphId"], "graph-a");
        assert_eq!(value["predicate"], "supports");
        assert_eq!(value["predicateLabel"], "supports");
        assert_eq!(value["sourceTitle"], "Source");
        assert_eq!(value["targetTitle"], "Target");
        assert_eq!(value["sourceBlockId"], "block-a");
        assert_eq!(value["targetSnippet"], "target text");
        assert_eq!(value["bidirectional"], true);
    }
}
