//! Wires drawn on a scene: the typed payload and the one writer the whiteboard's `wire` move calls (SPRINT D2, D9).
//!
//! A wire used to join documents and blocks. A scene wire may also end at an object IRI (a typed object on a board
//! as a card), and it remembers the scene element it came from. The stored wire (the `wires` map of the workspace
//! Y.Doc and the snapshot JSON built from it) carries, for each end, EITHER `sourceDocumentId`/`targetDocumentId`
//! (with the optional block id) OR `sourceIri`/`targetIri` and no document id for that end. Scene provenance is
//! `sceneGraphId`, `sceneDocumentId` (the board; new), `sceneElementId` (the arrow), `sceneSourceElementId`,
//! `sceneTargetElementId` and `sceneStableKey`; `sceneArtifactId` stays null for boards.
//!
//! Owned by G3 (INTERFACES.md). The signatures below are FINAL; G3 replaces the stub bodies.

use crate::app_runtime::AppHandle;
use crate::crdt_projection_flush::flush_graph_projection;
use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
use crate::rdf_workspace_materializer::{workspace_entity_unprojectable_iri, WorkspaceEntityKind};
use crate::rdf_workspace_terms::is_absolute_resource_iri;

/// One end of a scene wire. Exactly one of `document_id` and `iri` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SceneWireEnd {
    /// A document of `graph_id` (a note, or the board itself when the end is a shape drawn on it).
    pub(crate) document_id: Option<String>,
    /// For a document end: the block. For a shape of the board the block id is the element id.
    pub(crate) block_id: Option<String>,
    /// An absolute object IRI (a typed object, a card).
    pub(crate) iri: Option<String>,
    /// The graph the end lives in; `None` means the board's graph.
    pub(crate) graph_id: Option<String>,
    /// A snapshot title for an IRI end (the card's label); document ends are titled from the workspace.
    pub(crate) label: Option<String>,
}

/// Where on the scene the wire came from. Stored as the wire's `scene*` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SceneWireProvenance {
    /// `sceneGraphId`: the board's graph.
    pub(crate) graph_id: String,
    /// `sceneDocumentId`: the board's document id.
    pub(crate) document_id: String,
    /// `sceneElementId`: the arrow's element id.
    pub(crate) element_id: String,
    /// `sceneSourceElementId` / `sceneTargetElementId`: the shapes the arrow joins (bound text resolved to its container).
    pub(crate) source_element_id: String,
    pub(crate) target_element_id: String,
    /// `sceneStableKey`: `wb:{document_id}:{element_id}`.
    pub(crate) stable_key: String,
}

/// What the whiteboard's `wire` move hands to the writer: a complete wire, ids already chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SceneWireDraft {
    /// Chosen by the move from its seeded rng, so a replay of the same call names the same wire.
    pub(crate) wire_id: String,
    /// A short name (`supports`) or a predicate IRI, as `create_wires` takes it.
    pub(crate) predicate: String,
    pub(crate) source: SceneWireEnd,
    pub(crate) target: SceneWireEnd,
    pub(crate) scene: SceneWireProvenance,
}

/// What the writer reports back for each wire it stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SceneWireCreated {
    pub(crate) wire_id: String,
    /// The predicate as stored (the short name or IRI the draft carried).
    pub(crate) predicate: String,
}

/// Store `drafts` as workspace wires in `graph_id`, in order, and flush the workspace projection once at the end.
/// It goes through the same `workspace.createWire` queue operation `create_wires` uses, so every projection (snapshot,
/// workspace lane, wire lane) sees the wire. A draft whose `wire_id` already exists is replaced, not duplicated.
/// An invalid draft (an end with neither or both of `document_id` and `iri`, an empty predicate) is refused before
/// anything is written. Used by `whiteboard_apply`'s `wire` move.
pub(crate) async fn create_scene_wires(
    app: &AppHandle,
    graph_id: &str,
    drafts: &[SceneWireDraft],
) -> Result<Vec<SceneWireCreated>, String> {
    // Every draft is built and checked before the first is written: one bad draft in three writes none.
    let prepared = drafts
        .iter()
        .enumerate()
        .map(|(index, draft)| {
            scene_wire_operation(graph_id, draft)
                .map_err(|reason| format!("wire {} ({}): {reason}", index + 1, draft.wire_id))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if prepared.is_empty() {
        return Ok(Vec::new());
    }

    let mut created = Vec::with_capacity(prepared.len());
    for ((queue_document_id, payload), draft) in prepared.into_iter().zip(drafts) {
        enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.createWire".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(queue_document_id),
                payload,
            },
        )
        .await?;
        created.push(SceneWireCreated {
            wire_id: draft.wire_id.clone(),
            predicate: draft.predicate.clone(),
        });
    }
    flush_graph_projection(app.clone(), graph_id).await?;
    Ok(created)
}

/// Check one end: exactly one of a document and an object IRI; an object IRI is absolute and carries no block.
fn validate_end(side: &str, end: &SceneWireEnd) -> Result<(), String> {
    let document_id = end
        .document_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let iri = end
        .iri
        .as_deref()
        .map(str::trim)
        .filter(|iri| !iri.is_empty());
    match (document_id, iri) {
        (Some(_), Some(_)) => Err(format!(
            "the {side} is a document or an object IRI, not both"
        )),
        (None, None) => Err(format!("the {side} needs a document id or an object IRI")),
        (Some(_), None) => Ok(()),
        (None, Some(iri)) => {
            if end
                .block_id
                .as_deref()
                .is_some_and(|block| !block.trim().is_empty())
            {
                return Err(format!(
                    "the {side} is an object IRI and cannot name a block"
                ));
            }
            if !is_absolute_resource_iri(iri) {
                return Err(format!("the {side} IRI {iri:?} is not an absolute IRI"));
            }
            Ok(())
        }
    }
}

fn trimmed(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The `workspace.createWire` operation of one draft: the queue's document id (the source document, else the
/// board for an object source) and the camelCase payload. Pure: no store, no clock. Refuses a draft the writer
/// could not store, including one whose ids would make no IRI in the projection.
fn scene_wire_operation(
    graph_id: &str,
    draft: &SceneWireDraft,
) -> Result<(String, serde_json::Value), String> {
    let wire_id = draft.wire_id.trim();
    if wire_id.is_empty() {
        return Err("the wire id is empty".to_string());
    }
    let predicate = draft.predicate.trim();
    if predicate.is_empty() {
        return Err("the predicate is empty".to_string());
    }
    validate_end("source", &draft.source)?;
    validate_end("target", &draft.target)?;
    // A scene wire's source is stored in the board's own graph: the workspace that holds the wire is the one
    // whose document the source names.
    if let Some(source_graph) = draft.source.graph_id.as_deref() {
        if source_graph != graph_id {
            return Err(format!(
                "the source lives in graph {source_graph:?}, not in the board's graph {graph_id:?}"
            ));
        }
    }
    let scene = &draft.scene;
    for (name, value) in [
        ("scene.graph_id", &scene.graph_id),
        ("scene.document_id", &scene.document_id),
        ("scene.element_id", &scene.element_id),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{name} is empty"));
        }
    }

    let source_document_id = trimmed(&draft.source.document_id);
    let target_document_id = trimmed(&draft.target.document_id);
    let source_iri = trimmed(&draft.source.iri);
    let target_iri = trimmed(&draft.target.iri);
    let queue_document_id = source_document_id
        .clone()
        .unwrap_or_else(|| scene.document_id.clone());
    let target_graph_id = draft
        .target
        .graph_id
        .clone()
        .unwrap_or_else(|| graph_id.to_string());

    // An IRI end holds its label as the title; a document end is titled from the workspace by the writer.
    let mut payload = serde_json::json!({
        "wireId": wire_id,
        "sourceDocumentId": source_document_id,
        "sourceBlockId": trimmed(&draft.source.block_id).filter(|_| source_iri.is_none()),
        "targetGraphId": target_graph_id,
        "targetDocumentId": target_document_id,
        "targetBlockId": trimmed(&draft.target.block_id).filter(|_| target_iri.is_none()),
        "predicate": predicate,
        "bidirectional": false,
        "sceneGraphId": scene.graph_id,
        "sceneDocumentId": scene.document_id,
        "sceneElementId": scene.element_id,
        "sceneSourceElementId": scene.source_element_id,
        "sceneTargetElementId": scene.target_element_id,
        "sceneStableKey": scene.stable_key,
    });
    if let Some(iri) = &source_iri {
        payload["sourceIri"] = serde_json::json!(iri);
        if let Some(label) = trimmed(&draft.source.label) {
            payload["sourceTitle"] = serde_json::json!(label);
        }
    }
    if let Some(iri) = &target_iri {
        payload["targetIri"] = serde_json::json!(iri);
        if let Some(label) = trimmed(&draft.target.label) {
            payload["targetTitle"] = serde_json::json!(label);
        }
    }

    // The ids and IRIs must make IRIs in the projection: asked here, before anything is written, with the
    // entity the writer would hand the projector.
    if let Some(bad) = workspace_entity_unprojectable_iri(
        graph_id,
        WorkspaceEntityKind::Wire,
        &serde_json::json!({
            "id": wire_id,
            "sourceDocumentId": payload["sourceDocumentId"],
            "targetDocumentId": payload["targetDocumentId"],
            "sourceBlockId": payload["sourceBlockId"],
            "targetBlockId": payload["targetBlockId"],
            "sourceIri": payload.get("sourceIri"),
            "targetIri": payload.get("targetIri"),
            "predicate": predicate,
        }),
    ) {
        return Err(format!("{bad:?} is not an IRI; an id, a predicate or an IRI may not hold a space or any of <>\"{{}}|\\^`"));
    }
    Ok((queue_document_id, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECT: &str = "urn:mnemosyne:local:graph:wb-graph:dream:wolf-man";

    fn document_end(document_id: &str, block_id: Option<&str>) -> SceneWireEnd {
        SceneWireEnd {
            document_id: Some(document_id.to_string()),
            block_id: block_id.map(str::to_string),
            iri: None,
            graph_id: None,
            label: None,
        }
    }

    fn object_end(iri: &str, label: Option<&str>) -> SceneWireEnd {
        SceneWireEnd {
            document_id: None,
            block_id: None,
            iri: Some(iri.to_string()),
            graph_id: None,
            label: label.map(str::to_string),
        }
    }

    fn scene() -> SceneWireProvenance {
        SceneWireProvenance {
            graph_id: "wb-graph".into(),
            document_id: "wb-board".into(),
            element_id: "arw0000000000000000002".into(),
            source_element_id: "lnk0000000000000000001".into(),
            target_element_id: "crd0000000000000000001".into(),
            stable_key: "wb:wb-board:arw0000000000000000002".into(),
        }
    }

    fn draft(source: SceneWireEnd, target: SceneWireEnd) -> SceneWireDraft {
        SceneWireDraft {
            wire_id: "wire-wb-0001".into(),
            predicate: "supports".into(),
            source,
            target,
            scene: scene(),
        }
    }

    /// The fixture's promotion (INTERFACES.md section 5): a document source, an object target.
    #[test]
    fn a_document_to_object_draft_becomes_the_documented_payload() {
        let (queue_document, payload) = scene_wire_operation(
            "wb-graph",
            &draft(
                document_end("doc-traumdeutung", None),
                object_end(OBJECT, Some("The Wolf Man dream")),
            ),
        )
        .expect("valid");
        assert_eq!(queue_document, "doc-traumdeutung");
        assert_eq!(payload["wireId"], "wire-wb-0001");
        assert_eq!(payload["sourceDocumentId"], "doc-traumdeutung");
        assert!(payload["targetDocumentId"].is_null());
        assert_eq!(payload["targetIri"], OBJECT);
        assert_eq!(payload["targetTitle"], "The Wolf Man dream");
        assert_eq!(payload["targetGraphId"], "wb-graph");
        assert_eq!(payload["predicate"], "supports");
        assert_eq!(payload["bidirectional"], false);
        assert_eq!(payload["sceneGraphId"], "wb-graph");
        assert_eq!(payload["sceneDocumentId"], "wb-board");
        assert_eq!(payload["sceneElementId"], "arw0000000000000000002");
        assert_eq!(payload["sceneSourceElementId"], "lnk0000000000000000001");
        assert_eq!(payload["sceneTargetElementId"], "crd0000000000000000001");
        assert_eq!(
            payload["sceneStableKey"],
            "wb:wb-board:arw0000000000000000002"
        );
        assert!(payload.get("sourceIri").is_none());
    }

    /// An object source rides on the board: the queue's document is the board, and a shape of the board is a
    /// document end whose block is the element id.
    #[test]
    fn an_object_source_is_queued_on_the_board_and_a_shape_is_a_block_of_it() {
        let (queue_document, payload) = scene_wire_operation(
            "wb-graph",
            &draft(object_end(OBJECT, None), document_end("doc-b", Some("blk"))),
        )
        .expect("valid");
        assert_eq!(queue_document, "wb-board");
        assert_eq!(payload["sourceIri"], OBJECT);
        assert!(payload["sourceDocumentId"].is_null());
        assert_eq!(payload["targetDocumentId"], "doc-b");
        assert_eq!(payload["targetBlockId"], "blk");

        let (queue_document, payload) = scene_wire_operation(
            "wb-graph",
            &draft(
                document_end("wb-board", Some("box0000000000000000001")),
                document_end("wb-board", Some("note000000000000000001")),
            ),
        )
        .expect("valid");
        assert_eq!(queue_document, "wb-board");
        assert_eq!(payload["sourceBlockId"], "box0000000000000000001");
        assert_eq!(payload["targetBlockId"], "note000000000000000001");
    }

    #[test]
    fn invalid_drafts_are_refused_with_a_reason() {
        let ok_source = || document_end("doc-a", None);
        let ok_target = || document_end("doc-b", None);
        let check = |d: SceneWireDraft, needle: &str| {
            let error = scene_wire_operation("wb-graph", &d).unwrap_err();
            assert!(error.contains(needle), "{error:?} lacks {needle:?}");
        };
        // neither / both on an end
        check(
            draft(
                SceneWireEnd {
                    document_id: None,
                    block_id: None,
                    iri: None,
                    graph_id: None,
                    label: None,
                },
                ok_target(),
            ),
            "needs a document id or an object IRI",
        );
        check(
            draft(
                SceneWireEnd {
                    document_id: Some("doc-a".into()),
                    block_id: None,
                    iri: Some(OBJECT.into()),
                    graph_id: None,
                    label: None,
                },
                ok_target(),
            ),
            "not both",
        );
        check(
            draft(ok_source(), object_end("not-absolute", None)),
            "not an absolute IRI",
        );
        check(
            draft(ok_source(), object_end("urn:has space", None)),
            "not an absolute IRI",
        );
        let mut with_block = object_end(OBJECT, None);
        with_block.block_id = Some("b".into());
        check(draft(ok_source(), with_block), "cannot name a block");
        // predicate and id
        let mut empty_predicate = draft(ok_source(), ok_target());
        empty_predicate.predicate = "  ".into();
        check(empty_predicate, "predicate is empty");
        let mut empty_id = draft(ok_source(), ok_target());
        empty_id.wire_id.clear();
        check(empty_id, "wire id is empty");
        // an id that would make no IRI in the projection
        let mut spaced = draft(ok_source(), ok_target());
        spaced.wire_id = "wire with space".into();
        check(spaced, "is not an IRI");
        let mut spaced_predicate = draft(ok_source(), ok_target());
        spaced_predicate.predicate = "has space".into();
        check(spaced_predicate, "is not an IRI");
        // the source is in the board's graph
        let mut elsewhere = ok_source();
        elsewhere.graph_id = Some("other".into());
        check(draft(elsewhere, ok_target()), "not in the board's graph");
        // scene
        let mut no_board = draft(ok_source(), ok_target());
        no_board.scene.document_id.clear();
        check(no_board, "scene.document_id is empty");
    }
}
