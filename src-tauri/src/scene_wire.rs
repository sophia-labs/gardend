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
    let _ = (app, graph_id, drafts);
    unimplemented!("G3")
}
