use crate::app_runtime::AppHandle;
use crate::{
    document_projection_service::{hosted_document_response, hosted_document_summaries},
    hosted_navigation_projection::hosted_navigation_parts,
    json_utils::json_string,
    runtime_config::MDOC_NS,
};

pub(super) fn hosted_entity_type_infos() -> serde_json::Value {
    serde_json::json!({
        "types": [
            {
                "id": "document",
                "rdf_type": format!("{MDOC_NS}Document"),
                "cascade_policy": "automatic",
                "supports_websocket": true,
            },
            {
                "id": "folder",
                "rdf_type": format!("{MDOC_NS}Folder"),
                "cascade_policy": "block",
                "supports_websocket": false,
            },
            {
                "id": "artifact",
                "rdf_type": format!("{MDOC_NS}Artifact"),
                "cascade_policy": "none",
                "supports_websocket": false,
            },
        ],
    })
}

pub(super) fn validate_hosted_entity_type(entity_type: &str) -> Result<(), String> {
    match entity_type {
        "document" | "folder" | "artifact" => Ok(()),
        _ => Err(format!("Entity type '{entity_type}' not found")),
    }
}

fn hosted_entity_collection(
    app: &AppHandle,
    graph_id: &str,
    entity_type: &str,
) -> Result<Vec<serde_json::Value>, String> {
    validate_hosted_entity_type(entity_type)?;
    match entity_type {
        "document" => hosted_document_summaries(app, graph_id).map(|summaries| {
            summaries
                .into_iter()
                .map(|summary| {
                    serde_json::to_value(summary).expect("HostedDocumentSummary always serializes")
                })
                .collect()
        }),
        "folder" => hosted_navigation_parts(app, graph_id).map(|(folders, _, _)| folders),
        "artifact" => hosted_navigation_parts(app, graph_id).map(|(_, _, artifacts)| artifacts),
        _ => unreachable!(),
    }
}

pub(super) fn hosted_entity_list_response(
    app: &AppHandle,
    graph_id: &str,
    entity_type: &str,
    limit: usize,
    offset: usize,
) -> Result<serde_json::Value, String> {
    let entities = hosted_entity_collection(app, graph_id, entity_type)?;
    let total = entities.len();
    let data = entities
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "data": data,
        "meta": {
            "total": total,
            "limit": limit,
            "offset": offset,
            "entity_type": entity_type,
        },
    }))
}

pub(super) fn hosted_entity_response(
    app: &AppHandle,
    graph_id: &str,
    entity_type: &str,
    entity_id: &str,
) -> Result<serde_json::Value, String> {
    validate_hosted_entity_type(entity_type)?;
    match entity_type {
        "document" => hosted_document_response(app, graph_id, entity_id).map(|envelope| {
            serde_json::to_value(envelope).expect("HostedDocumentEnvelope always serializes")
        }),
        "folder" | "artifact" => hosted_entity_collection(app, graph_id, entity_type)?
            .into_iter()
            .find(|entity| json_string(entity.get("id")).as_deref() == Some(entity_id))
            .ok_or_else(|| format!("{entity_type} {entity_id} not found")),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_entity_types_preserve_public_contract() {
        let value = hosted_entity_type_infos();
        let types = value["types"].as_array().unwrap();

        assert_eq!(types.len(), 3);
        assert_eq!(types[0]["id"], "document");
        assert_eq!(types[0]["cascade_policy"], "automatic");
        assert_eq!(types[0]["supports_websocket"], true);
        assert_eq!(types[1]["id"], "folder");
        assert_eq!(types[1]["cascade_policy"], "block");
        assert_eq!(types[2]["id"], "artifact");
        assert_eq!(types[2]["cascade_policy"], "none");
    }

    #[test]
    fn entity_type_validation_accepts_known_types_only() {
        assert!(validate_hosted_entity_type("document").is_ok());
        assert!(validate_hosted_entity_type("folder").is_ok());
        assert!(validate_hosted_entity_type("artifact").is_ok());
        assert_eq!(
            validate_hosted_entity_type("wire").unwrap_err(),
            "Entity type 'wire' not found"
        );
    }
}
