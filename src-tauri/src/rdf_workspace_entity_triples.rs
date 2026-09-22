use crate::{
    json_utils::json_string,
    rdf::{document_subject, push_string_triple, push_uri_triple, RdfTriple},
    rdf_workspace_terms::{document_ref_uri, wire_predicate_uri, workspace_entity_subject},
    rdf_workspace_values::{
        extra_value, push_document_source_file_triples, push_parent_folder_triple,
        push_workspace_extra_triples, push_workspace_number_triple, push_workspace_value_triple,
    },
    runtime_config::{DCTERMS_NS, MDOC_NS, NFO_NS, NIE_NS, RDF_TYPE, WIRE_NS, XSD_NS},
};

pub(super) fn push_folder_snapshot_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    folder: &serde_json::Value,
) {
    let Some(id) = json_string(folder.get("id")) else {
        return;
    };
    let subject = workspace_entity_subject(graph_id, "folder", &id);
    push_uri_triple(triples, &subject, RDF_TYPE, &format!("{MDOC_NS}Folder"));
    if let Some(name) = json_string(folder.get("name")) {
        push_string_triple(triples, &subject, &format!("{NFO_NS}fileName"), &name);
    }
    push_parent_folder_triple(triples, graph_id, &subject, folder);
    push_workspace_number_triple(
        triples,
        &subject,
        &format!("{MDOC_NS}order"),
        folder,
        "order",
    );
    if let Some(section) = json_string(folder.get("section")) {
        push_string_triple(triples, &subject, &format!("{MDOC_NS}section"), &section);
    }
    push_workspace_extra_triples(
        triples,
        graph_id,
        &subject,
        folder,
        &[
            ("createdAt", format!("{MDOC_NS}createdAt"), None),
            ("updatedAt", format!("{MDOC_NS}updatedAt"), None),
        ],
    );
}

pub(super) fn push_document_snapshot_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    document: &serde_json::Value,
) {
    let Some(id) = json_string(document.get("id")) else {
        return;
    };
    let subject = document_subject(&id);
    push_uri_triple(
        triples,
        &subject,
        RDF_TYPE,
        &format!("{MDOC_NS}TipTapDocument"),
    );
    // Flow board (unit G3): emit `flow:Board` BESIDE `doc:TipTapDocument`,
    // plus the discriminator literal `flow:documentKind "flow-board"`. Beside,
    // not instead — the workspace RECONCILE partitions desired triples into
    // exactly the 10 known rdf:type class spans, so a subject typed ONLY
    // `flow:Board` would route to no span and never materialize; with the
    // mdoc type kept, the board rides the TipTapDocument span exactly the way
    // an artifact's kind class (`mdoc:Image` etc.) rides the Artifact span
    // (see `rdf_workspace_store_materializer.rs::partition_workspace_desired`).
    // `documentKind` reaches this snapshot entity as a generic extra field
    // (`insert_extra_fields`), hence the `extra_value` fallback.
    let document_kind = json_string(
        document
            .get("documentKind")
            .or_else(|| extra_value(document, "documentKind")),
    );
    if document_kind.as_deref() == Some(crate::flow_board::FLOW_BOARD_KIND) {
        push_uri_triple(
            triples,
            &subject,
            RDF_TYPE,
            &format!("{}Board", crate::flow_board::FLOW_NS),
        );
        push_string_triple(
            triples,
            &subject,
            &format!("{}documentKind", crate::flow_board::FLOW_NS),
            crate::flow_board::FLOW_BOARD_KIND,
        );
    }
    if let Some(title) = json_string(document.get("title")) {
        push_string_triple(triples, &subject, &format!("{DCTERMS_NS}title"), &title);
    }
    push_parent_folder_triple(triples, graph_id, &subject, document);
    push_workspace_number_triple(
        triples,
        &subject,
        &format!("{MDOC_NS}order"),
        document,
        "order",
    );
    if let Some(section) = json_string(document.get("section")) {
        push_string_triple(triples, &subject, &format!("{MDOC_NS}section"), &section);
    }
    push_workspace_value_triple(
        triples,
        &subject,
        &format!("{MDOC_NS}readOnly"),
        document.get("readOnly"),
        Some(&format!("{XSD_NS}boolean")),
    );
    push_workspace_value_triple(
        triples,
        &subject,
        &format!("{MDOC_NS}createdAt"),
        document
            .get("createdAt")
            .or_else(|| extra_value(document, "createdAt")),
        None,
    );
    push_workspace_value_triple(
        triples,
        &subject,
        &format!("{MDOC_NS}updatedAt"),
        document
            .get("updatedAt")
            .or_else(|| extra_value(document, "updatedAt")),
        None,
    );
    push_workspace_extra_triples(
        triples,
        graph_id,
        &subject,
        document,
        &[
            ("description", format!("{DCTERMS_NS}description"), None),
            ("describedAt", format!("{MDOC_NS}describedAt"), None),
            ("lastAccessedAt", format!("{MDOC_NS}lastAccessedAt"), None),
        ],
    );
    push_document_source_file_triples(triples, &subject, document);
}

pub(super) fn push_artifact_snapshot_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    artifact: &serde_json::Value,
) {
    let Some(id) = json_string(artifact.get("id")) else {
        return;
    };
    let subject = workspace_entity_subject(graph_id, "artifact", &id);
    push_uri_triple(triples, &subject, RDF_TYPE, &format!("{MDOC_NS}Artifact"));
    // Graph-canonical artifact typing: also emit the specific kind class so the
    // artifact is SPARQL-discoverable (e.g. `?a a mdoc:Image`). Kind is derived
    // deterministically from mime, so existing artifacts type with no migration.
    let mime = json_string(artifact.get("mimeType")).unwrap_or_default();
    let file_type = json_string(artifact.get("fileType"));
    let kind = crate::artifact_kinds::kind_from_mime(&mime, file_type.as_deref());
    push_uri_triple(
        triples,
        &subject,
        RDF_TYPE,
        &format!("{MDOC_NS}{}", crate::artifact_kinds::kind_class(kind)),
    );
    push_string_triple(triples, &subject, &format!("{MDOC_NS}kind"), kind);
    if let Some(name) = json_string(artifact.get("name")) {
        push_string_triple(triples, &subject, &format!("{NFO_NS}fileName"), &name);
    }
    push_parent_folder_triple(triples, graph_id, &subject, artifact);
    push_workspace_number_triple(
        triples,
        &subject,
        &format!("{MDOC_NS}order"),
        artifact,
        "order",
    );
    if let Some(mime_type) = json_string(artifact.get("mimeType")) {
        push_string_triple(triples, &subject, &format!("{NIE_NS}mimeType"), &mime_type);
    }
    if let Some(status) = json_string(artifact.get("status")) {
        push_string_triple(triples, &subject, &format!("{MDOC_NS}status"), &status);
    }
    push_workspace_value_triple(
        triples,
        &subject,
        &format!("{NFO_NS}fileSize"),
        artifact
            .get("size")
            .or_else(|| extra_value(artifact, "size")),
        Some(&format!("{XSD_NS}integer")),
    );
    if let Some(ingested_doc_id) = json_string(
        artifact
            .get("ingestedDocId")
            .or_else(|| extra_value(artifact, "ingestedDocId")),
    ) {
        push_uri_triple(
            triples,
            &subject,
            &format!("{MDOC_NS}ingestedDocId"),
            &document_subject(&ingested_doc_id),
        );
    }
    push_workspace_extra_triples(
        triples,
        graph_id,
        &subject,
        artifact,
        &[
            ("fileType", format!("{MDOC_NS}fileType"), None),
            ("errorMessage", format!("{MDOC_NS}errorMessage"), None),
            ("storageKey", format!("{MDOC_NS}storageKey"), None),
            ("contentHashSha256", format!("{MDOC_NS}contentHashSha256"), None),
            ("contentOperationId", format!("{MDOC_NS}contentOperationId"), None),
            (
                "originalFilename",
                format!("{MDOC_NS}originalFilename"),
                None,
            ),
            (
                "sceneProjectionText",
                format!("{MDOC_NS}sceneProjectionText"),
                None,
            ),
            (
                "sceneProjectedAt",
                format!("{MDOC_NS}sceneProjectedAt"),
                None,
            ),
            ("createdAt", format!("{MDOC_NS}createdAt"), None),
            ("updatedAt", format!("{MDOC_NS}updatedAt"), None),
        ],
    );
    if let Some(scene_projection) = artifact
        .get("sceneProjection")
        .or_else(|| extra_value(artifact, "sceneProjection"))
    {
        push_scene_projection_triples(triples, graph_id, &id, &subject, scene_projection);
    }
}

pub(super) fn push_wire_snapshot_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    wire: &serde_json::Value,
) {
    // SINGLE SOURCE OF TRUTH: delegate to `wire_subject_triples` (type head +
    // body) so the wholesale workspace path and the reconcile Wire span project
    // the identical per-wire set — no drift between the two paths.
    crate::rdf_wire_materializer::wire_subject_triples(triples, graph_id, wire);
}

fn push_scene_projection_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    artifact_id: &str,
    artifact_subject: &str,
    scene_projection: &serde_json::Value,
) {
    let Some(scene_projection) = scene_projection.as_object() else {
        return;
    };
    let scene_subject = scene_subject(graph_id, artifact_id);
    push_uri_triple(
        triples,
        artifact_subject,
        &format!("{MDOC_NS}sceneProjection"),
        &scene_subject,
    );
    push_uri_triple(
        triples,
        &scene_subject,
        RDF_TYPE,
        &format!("{MDOC_NS}SceneProjection"),
    );
    push_uri_triple(
        triples,
        &scene_subject,
        &format!("{MDOC_NS}sceneArtifact"),
        artifact_subject,
    );
    for (field, predicate) in [
        ("projectedAt", format!("{MDOC_NS}sceneProjectedAt")),
        ("searchText", format!("{MDOC_NS}sceneProjectionText")),
    ] {
        push_workspace_value_triple(
            triples,
            &scene_subject,
            &predicate,
            scene_projection.get(field),
            // The workspace `emporium-workspace` contract types every entity
            // timestamp as xsd:string (epoch-ms / opaque values are NOT valid
            // xsd:dateTime lexical forms); emit these as plain string literals.
            None,
        );
    }

    push_scene_anchor_triples(
        triples,
        graph_id,
        artifact_id,
        &scene_subject,
        scene_projection
            .get("anchors")
            .and_then(serde_json::Value::as_array),
    );
    push_scene_frame_triples(
        triples,
        graph_id,
        artifact_id,
        &scene_subject,
        scene_projection
            .get("frames")
            .and_then(serde_json::Value::as_array),
    );
    push_scene_text_triples(
        triples,
        graph_id,
        artifact_id,
        &scene_subject,
        scene_projection
            .get("text")
            .and_then(serde_json::Value::as_array),
    );
    push_scene_wire_candidate_triples(
        triples,
        graph_id,
        artifact_id,
        &scene_subject,
        scene_projection
            .get("wireCandidates")
            .and_then(serde_json::Value::as_array),
    );
    push_scene_diagnostic_triples(
        triples,
        graph_id,
        artifact_id,
        &scene_subject,
        scene_projection
            .get("diagnostics")
            .and_then(serde_json::Value::as_array),
    );
}

fn push_scene_anchor_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    artifact_id: &str,
    scene_subject: &str,
    anchors: Option<&Vec<serde_json::Value>>,
) {
    let Some(anchors) = anchors else {
        return;
    };
    for anchor in anchors {
        let Some(scene_element_id) = json_string(anchor.get("sceneElementId")) else {
            continue;
        };
        let subject = scene_child_subject(graph_id, artifact_id, "anchor", &scene_element_id);
        push_scene_child_base_triples(
            triples,
            scene_subject,
            &subject,
            "sceneAnchor",
            "SceneAnchor",
            &scene_element_id,
        );
        if let Some(title) = json_string(anchor.get("title")) {
            push_string_triple(triples, &subject, &format!("{DCTERMS_NS}title"), &title);
        }
        if let Some(workspace_title) = json_string(anchor.get("workspaceTitle")) {
            push_string_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}workspaceTitle"),
                &workspace_title,
            );
        }
        push_workspace_value_triple(
            triples,
            &subject,
            &format!("{MDOC_NS}targetExists"),
            anchor.get("targetExists"),
            Some(&format!("{XSD_NS}boolean")),
        );
        if let Some(kind) = json_string(anchor.get("kind")) {
            push_string_triple(triples, &subject, &format!("{MDOC_NS}targetKind"), &kind);
        }
        if let Some(wire_document_id) = json_string(anchor.get("wireDocumentId")) {
            push_string_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}wireDocumentId"),
                &wire_document_id,
            );
            push_uri_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}wireDocument"),
                &document_ref_uri(&wire_document_id),
            );
        }
        if let Some(target_graph_id) = json_string(anchor.get("graphId")) {
            push_string_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}targetGraphId"),
                &target_graph_id,
            );
            if let Some(target_id) = json_string(anchor.get("id")) {
                push_string_triple(triples, &subject, &format!("{MDOC_NS}targetId"), &target_id);
                match json_string(anchor.get("kind")).as_deref() {
                    Some("document") => push_uri_triple(
                        triples,
                        &subject,
                        &format!("{MDOC_NS}anchorsDocument"),
                        &document_ref_uri(&target_id),
                    ),
                    Some("artifact") => push_uri_triple(
                        triples,
                        &subject,
                        &format!("{MDOC_NS}anchorsArtifact"),
                        &workspace_entity_subject(&target_graph_id, "artifact", &target_id),
                    ),
                    _ => {}
                }
            }
        }
        if let Some(frame_id) = json_string(anchor.get("frameId")) {
            push_uri_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}inSceneFrame"),
                &scene_child_subject(graph_id, artifact_id, "frame", &frame_id),
            );
        }
    }
}

fn push_scene_frame_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    artifact_id: &str,
    scene_subject: &str,
    frames: Option<&Vec<serde_json::Value>>,
) {
    let Some(frames) = frames else {
        return;
    };
    for frame in frames {
        let Some(scene_element_id) = json_string(frame.get("sceneElementId")) else {
            continue;
        };
        let subject = scene_child_subject(graph_id, artifact_id, "frame", &scene_element_id);
        push_scene_child_base_triples(
            triples,
            scene_subject,
            &subject,
            "sceneFrame",
            "SceneFrame",
            &scene_element_id,
        );
        if let Some(title) = json_string(frame.get("title")) {
            push_string_triple(triples, &subject, &format!("{DCTERMS_NS}title"), &title);
        }
    }
}

fn push_scene_text_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    artifact_id: &str,
    scene_subject: &str,
    text_items: Option<&Vec<serde_json::Value>>,
) {
    let Some(text_items) = text_items else {
        return;
    };
    for text in text_items {
        let Some(scene_element_id) = json_string(text.get("sceneElementId")) else {
            continue;
        };
        let subject = scene_child_subject(graph_id, artifact_id, "text", &scene_element_id);
        push_scene_child_base_triples(
            triples,
            scene_subject,
            &subject,
            "sceneText",
            "SceneText",
            &scene_element_id,
        );
        if let Some(content) = json_string(text.get("text")) {
            push_string_triple(triples, &subject, &format!("{MDOC_NS}content"), &content);
        }
        if let Some(container_id) = json_string(text.get("containerId")) {
            push_uri_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}sceneContainer"),
                &scene_child_subject(graph_id, artifact_id, "element", &container_id),
            );
        }
        if let Some(frame_id) = json_string(text.get("frameId")) {
            push_uri_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}inSceneFrame"),
                &scene_child_subject(graph_id, artifact_id, "frame", &frame_id),
            );
        }
    }
}

fn push_scene_wire_candidate_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    artifact_id: &str,
    scene_subject: &str,
    wire_candidates: Option<&Vec<serde_json::Value>>,
) {
    let Some(wire_candidates) = wire_candidates else {
        return;
    };
    for candidate in wire_candidates {
        let Some(scene_element_id) = json_string(candidate.get("sceneElementId")) else {
            continue;
        };
        let subject =
            scene_child_subject(graph_id, artifact_id, "wire-candidate", &scene_element_id);
        push_scene_child_base_triples(
            triples,
            scene_subject,
            &subject,
            "sceneWireCandidate",
            "SceneWireCandidate",
            &scene_element_id,
        );
        if let Some(source_document_id) = json_string(candidate.get("sourceDocumentId")) {
            push_uri_triple(
                triples,
                &subject,
                &format!("{WIRE_NS}sourceDocument"),
                &document_ref_uri(&source_document_id),
            );
        }
        if let Some(source_kind) = json_string(candidate.get("sourceKind")) {
            push_string_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}sourceKind"),
                &source_kind,
            );
        }
        if let Some(source_id) = json_string(candidate.get("sourceId")) {
            push_string_triple(triples, &subject, &format!("{MDOC_NS}sourceId"), &source_id);
        }
        if let Some(target_document_id) = json_string(candidate.get("targetDocumentId")) {
            push_uri_triple(
                triples,
                &subject,
                &format!("{WIRE_NS}targetDocument"),
                &document_ref_uri(&target_document_id),
            );
        }
        if let Some(target_kind) = json_string(candidate.get("targetKind")) {
            push_string_triple(
                triples,
                &subject,
                &format!("{MDOC_NS}targetKind"),
                &target_kind,
            );
        }
        if let Some(target_id) = json_string(candidate.get("targetId")) {
            push_string_triple(triples, &subject, &format!("{MDOC_NS}targetId"), &target_id);
        }
        if let Some(target_graph_id) = json_string(candidate.get("targetGraphId")) {
            push_string_triple(
                triples,
                &subject,
                &format!("{WIRE_NS}targetGraph"),
                &target_graph_id,
            );
        }
        if let Some(predicate) = json_string(candidate.get("predicate")) {
            push_uri_triple(
                triples,
                &subject,
                &format!("{WIRE_NS}predicate"),
                &wire_predicate_uri(&predicate),
            );
        }
        if let Some(label) = json_string(candidate.get("label")) {
            push_string_triple(triples, &subject, &format!("{MDOC_NS}label"), &label);
        }
    }
}

fn push_scene_diagnostic_triples(
    triples: &mut Vec<RdfTriple>,
    graph_id: &str,
    artifact_id: &str,
    scene_subject: &str,
    diagnostics: Option<&Vec<serde_json::Value>>,
) {
    let Some(diagnostics) = diagnostics else {
        return;
    };
    for diagnostic in diagnostics {
        let Some(scene_element_id) = json_string(diagnostic.get("sceneElementId")) else {
            continue;
        };
        let code = json_string(diagnostic.get("code")).unwrap_or_else(|| "diagnostic".to_string());
        let subject = scene_child_subject(
            graph_id,
            artifact_id,
            "diagnostic",
            &format!("{scene_element_id}:{code}"),
        );
        push_scene_child_base_triples(
            triples,
            scene_subject,
            &subject,
            "sceneDiagnostic",
            "SceneDiagnostic",
            &scene_element_id,
        );
        push_string_triple(
            triples,
            &subject,
            &format!("{MDOC_NS}diagnosticCode"),
            &code,
        );
        if let Some(message) = json_string(diagnostic.get("message")) {
            push_string_triple(triples, &subject, &format!("{MDOC_NS}message"), &message);
        }
    }
}

fn push_scene_child_base_triples(
    triples: &mut Vec<RdfTriple>,
    scene_subject: &str,
    child_subject: &str,
    scene_predicate: &str,
    class_name: &str,
    scene_element_id: &str,
) {
    push_uri_triple(
        triples,
        scene_subject,
        &format!("{MDOC_NS}{scene_predicate}"),
        child_subject,
    );
    push_uri_triple(
        triples,
        child_subject,
        RDF_TYPE,
        &format!("{MDOC_NS}{class_name}"),
    );
    push_string_triple(
        triples,
        child_subject,
        &format!("{MDOC_NS}sceneElementId"),
        scene_element_id,
    );
}

fn scene_subject(graph_id: &str, artifact_id: &str) -> String {
    format!(
        "{}#scene",
        workspace_entity_subject(graph_id, "artifact", artifact_id)
    )
}

fn scene_child_subject(
    graph_id: &str,
    artifact_id: &str,
    child_type: &str,
    scene_element_id: &str,
) -> String {
    format!(
        "{}#scene-{child_type}-{}",
        workspace_entity_subject(graph_id, "artifact", artifact_id),
        uri_component(scene_element_id),
    )
}

fn uri_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdf::format_rdf_triple;

    /// Unit G3, brief item 2: a workspace document entry whose `documentKind`
    /// extra field is `"flow-board"` types as `flow:Board` (beside
    /// `doc:TipTapDocument` — see the partition-routing comment at the emit
    /// site) and carries `flow:documentKind "flow-board"`; a plain document
    /// gains neither.
    #[test]
    fn flow_board_document_entry_types_as_flow_board() {
        let board = serde_json::json!({
            "id": "flow-board",
            "title": "Mission Board",
            "order": 1,
            "extra": { "documentKind": "flow-board" }
        });
        let mut triples = Vec::new();
        push_document_snapshot_triples(&mut triples, "graph-a", &board);
        let rendered = triples
            .iter()
            .map(format_rdf_triple)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("<urn:sophia:flow:vocab:Board>"));
        assert!(rendered.contains("documentKind> \"flow-board\""));
        assert!(
            rendered.contains("TipTapDocument>"),
            "the mdoc type must stay beside flow:Board so the workspace \
             reconcile's class-span partition still routes the subject"
        );

        // Top-level (non-extra) carriage works too — both shapes appear
        // depending on which serializer produced the snapshot entity.
        let board_top = serde_json::json!({
            "id": "flow-board",
            "title": "Mission Board",
            "order": 1,
            "documentKind": "flow-board"
        });
        let mut top_triples = Vec::new();
        push_document_snapshot_triples(&mut top_triples, "graph-a", &board_top);
        let top_rendered = top_triples
            .iter()
            .map(format_rdf_triple)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(top_rendered.contains("<urn:sophia:flow:vocab:Board>"));

        let plain = serde_json::json!({
            "id": "doc-plain",
            "title": "Plain",
            "order": 2
        });
        let mut plain_triples = Vec::new();
        push_document_snapshot_triples(&mut plain_triples, "graph-a", &plain);
        let plain_rendered = plain_triples
            .iter()
            .map(format_rdf_triple)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!plain_rendered.contains("urn:sophia:flow:vocab"));
    }

    #[test]
    fn artifact_triples_include_scene_projection_summary_fields() {
        let artifact = serde_json::json!({
            "id": "artifact-scene",
            "name": "Map.excalidraw",
            "mimeType": "application/vnd.excalidraw+json",
            "status": "ready",
            "order": 1,
            "sceneProjectionText": "Anchor: Source\nWire: doc-a supports doc-b",
            "sceneProjectedAt": "2026-06-04T12:00:00.000Z",
            "sceneProjection": {
                "projectedAt": "2026-06-04T12:00:00.000Z",
                "searchText": "Anchor: Source\nWire: doc-a supports doc-b",
                "anchors": [{
                    "sceneElementId": "source-el",
                    "kind": "document",
                    "graphId": "graph-a",
                    "id": "doc-a",
                    "title": "Source",
                    "targetExists": true,
                    "workspaceTitle": "Source workspace title",
                    "wireDocumentId": "doc-a",
                    "frameId": "frame-1"
                }, {
                    "sceneElementId": "artifact el",
                    "kind": "artifact",
                    "graphId": "graph-a",
                    "id": "artifact-target",
                    "title": "Image",
                    "wireDocumentId": "doc-from-artifact"
                }],
                "frames": [{
                    "sceneElementId": "frame-1",
                    "title": "Research cluster"
                }],
                "text": [{
                    "sceneElementId": "note-1",
                    "text": "Important note",
                    "frameId": "frame-1"
                }],
                "wireCandidates": [{
                    "sceneElementId": "arrow-1",
                    "sourceKind": "document",
                    "sourceId": "doc-a",
                    "sourceDocumentId": "doc-a",
                    "targetGraphId": "graph-a",
                    "targetKind": "document",
                    "targetId": "doc-b",
                    "targetDocumentId": "doc-b",
                    "predicate": "supports",
                    "label": "supports"
                }],
                "diagnostics": [{
                    "sceneElementId": "bad-arrow",
                    "code": "arrow-unlinked-endpoint",
                    "message": "Arrow endpoint is not linked to a Mnemosyne node."
                }]
            }
        });
        let mut triples = Vec::new();

        push_artifact_snapshot_triples(&mut triples, "graph-a", &artifact);

        let rendered = triples
            .iter()
            .map(format_rdf_triple)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered
            .contains("sceneProjectionText> \"Anchor: Source\\nWire: doc-a supports doc-b\""));
        // sceneProjectedAt is now a PLAIN xsd:string literal (the emporium-workspace
        // contract types every entity timestamp as xsd:string — epoch-ms / opaque
        // values are not valid xsd:dateTime lexical forms). REGRESSION GUARD: the
        // value is present AND is NOT stamped with a `^^` datatype tag.
        assert!(rendered.contains("sceneProjectedAt> \"2026-06-04T12:00:00.000Z\" ."));
        assert!(
            !rendered.contains("sceneProjectedAt> \"2026-06-04T12:00:00.000Z\"^^"),
            "sceneProjectedAt must be a plain xsd:string literal, not a typed dateTime"
        );
        assert!(rendered.contains(
            "sceneProjection> <urn:mnemosyne:local:graph:graph-a:artifact:artifact-scene#scene>"
        ));
        assert!(rendered.contains("SceneProjection>"));
        assert!(rendered.contains("sceneAnchor> <urn:mnemosyne:local:graph:graph-a:artifact:artifact-scene#scene-anchor-source-el>"));
        assert!(rendered.contains("anchorsDocument> <urn:mnemosyne:local:document:doc-a>"));
        assert!(rendered.contains("wireDocumentId> \"doc-a\""));
        assert!(rendered.contains("wireDocument> <urn:mnemosyne:local:document:doc-a>"));
        assert!(rendered.contains("workspaceTitle> \"Source workspace title\""));
        assert!(rendered.contains("targetExists> \"true\"^^"));
        assert!(rendered.contains(
            "anchorsArtifact> <urn:mnemosyne:local:graph:graph-a:artifact:artifact-target>"
        ));
        assert!(rendered.contains("wireDocumentId> \"doc-from-artifact\""));
        assert!(rendered.contains("wireDocument> <urn:mnemosyne:local:document:doc-from-artifact>"));
        assert!(rendered.contains("scene-anchor-artifact%20el>"));
        assert!(rendered.contains("sceneFrame> <urn:mnemosyne:local:graph:graph-a:artifact:artifact-scene#scene-frame-frame-1>"));
        assert!(rendered.contains("sceneText> <urn:mnemosyne:local:graph:graph-a:artifact:artifact-scene#scene-text-note-1>"));
        assert!(rendered.contains("content> \"Important note\""));
        assert!(rendered.contains("sceneWireCandidate> <urn:mnemosyne:local:graph:graph-a:artifact:artifact-scene#scene-wire-candidate-arrow-1>"));
        assert!(rendered.contains("sourceKind> \"document\""));
        assert!(rendered.contains("targetKind> \"document\""));
        assert!(rendered.contains(&format!("predicate> <{WIRE_NS}supports>")));
        assert!(rendered.contains("sceneDiagnostic> <urn:mnemosyne:local:graph:graph-a:artifact:artifact-scene#scene-diagnostic-bad-arrow%3Aarrow-unlinked-endpoint>"));
        assert!(rendered.contains("diagnosticCode> \"arrow-unlinked-endpoint\""));
    }
}
