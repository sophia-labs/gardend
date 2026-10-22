//! A board's shapes as the document record's blocks (SPRINT D3), and the guard that keeps block-editing tools off boards.
//!
//! A whiteboard's saved record used to carry `blocks: []`. Now it lists one block per live shape (id = the element id),
//! so each shape gets its own semantic passage, `search_blocks` hit, wire endpoint (`{board}#block-{id}`) and wire
//! snippet through paths that already exist. Arrows, bound text and deleted elements are not blocks; frames are.
//!
//! Owned by G1. The signatures below are FINAL.

use super::outline::{self, Shape};
use super::scene::Scene;
use crate::app_runtime::AppHandle;
use crate::document_types::BlockSnapshot;
use std::collections::BTreeSet;

/// The words a shape contributes to its block: a card says `{class}: {label}` (just the label for a document card);
/// anything else says its text, then its link title after an em dash when the title adds words.
fn content_of(shape: &Shape) -> String {
    if let Some(card) = &shape.card {
        return if card.class_label.is_empty() {
            card.label.clone()
        } else {
            format!("{}: {}", card.class_label, card.label)
        };
    }
    match shape.link_title.as_deref() {
        Some(title) if !title.is_empty() && title != shape.text => {
            if shape.text.is_empty() {
                title.to_string()
            } else {
                format!("{} \u{2014} {title}", shape.text)
            }
        }
        _ => shape.text.clone(),
    }
}

/// The record's blocks for a scene, in reading order (top to bottom, then left to right, the order of
/// `outline::shapes`): `id` = element id, `block_type` = `scene::shape_kind`, `content` = the shape's words plus its
/// link title, `parent_id` = the frame's element id when the shape is inside a frame, `order` = 0.0, 1.0, 2.0, ...,
/// `level`/`checked`/`language` None, `marks` empty.
///
/// A pure function of the scene: no clock, no ids minted, so an unchanged board gives the same bytes.
pub(crate) fn blocks_of(scene: &Scene) -> Vec<BlockSnapshot> {
    let shapes = outline::shapes(scene);
    // A frame id that names a deleted or missing element is not a parent.
    let frames: BTreeSet<&str> = shapes
        .iter()
        .filter(|shape| shape.kind == "frame")
        .map(|shape| shape.id.as_str())
        .collect();
    shapes
        .iter()
        .enumerate()
        .map(|(index, shape)| BlockSnapshot {
            id: shape.id.clone(),
            block_type: shape.kind.to_string(),
            content: content_of(shape),
            parent_id: shape
                .frame
                .as_deref()
                .filter(|frame| frames.contains(frame))
                .map(str::to_string),
            order: index as f64,
            level: None,
            checked: None,
            language: None,
            marks: Vec::new(),
        })
        .collect()
}

/// Refuse a block-editing tool on a scene kind. Ok for any other document. The error for a whiteboard is exactly
/// `"{tool}: a whiteboard is drawn, not written: use whiteboard_apply"`; for a Flow board
/// `"{tool}: a Flow board is not a text document: use flow_seed_board"`.
pub(crate) async fn refuse_scene_kind(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    tool: &str,
) -> Result<(), String> {
    // A graph or workspace that cannot be read is the tool's own error to report, not this guard's.
    let kind = crate::crdt_engine::document_ops::resolve_document_kind(app, graph_id, document_id)
        .await
        .unwrap_or(None);
    match kind.as_deref() {
        Some(super::WHITEBOARD_KIND) => Err(format!(
            "{tool}: a whiteboard is drawn, not written: use whiteboard_apply"
        )),
        Some(crate::flow_board::FLOW_BOARD_KIND) => Err(format!(
            "{tool}: a Flow board is not a text document: use flow_seed_board"
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whiteboard::scene::Element;
    use serde_json::Value;
    use std::collections::BTreeMap;

    const V2: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/whiteboard/whiteboard-fixture-v2-semantic.json"
    ));

    fn v2_scene() -> Scene {
        let fixture: Value = serde_json::from_str(V2).expect("fixture parses");
        let mut map: BTreeMap<String, Element> = BTreeMap::new();
        for el in fixture["elements"].as_array().expect("elements") {
            let el = el.as_object().expect("element object").clone();
            map.insert(el["id"].as_str().expect("id").to_string(), el);
        }
        Scene::from_elements(map)
    }

    #[test]
    fn blocks_follow_reading_order() {
        let blocks = blocks_of(&v2_scene());
        let ids: Vec<&str> = blocks.iter().map(|b| b.id.as_str()).collect();
        // 40 px bands, then left to right: the two top shapes, the frame, then the card (y 290) before the box (y 300).
        assert_eq!(
            ids,
            vec![
                "box0000000000000000001",
                "note000000000000000001",
                "frm0000000000000000001",
                "crd0000000000000000001",
                "lnk0000000000000000001",
            ]
        );
        let orders: Vec<f64> = blocks.iter().map(|b| b.order).collect();
        assert_eq!(orders, vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        let kinds: Vec<&str> = blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(kinds, vec!["box", "note", "frame", "card", "box"]);
        assert_eq!(blocks[0].content, "Freud: the dream-work");
        assert!(blocks.iter().all(|b| b.marks.is_empty()
            && b.level.is_none()
            && b.checked.is_none()
            && b.language.is_none()));
        // The same scene gives the same bytes.
        assert_eq!(
            serde_json::to_string(&blocks).unwrap(),
            serde_json::to_string(&blocks_of(&v2_scene())).unwrap()
        );
    }

    #[test]
    fn frame_members_name_their_frame() {
        let blocks = blocks_of(&v2_scene());
        let parent = |id: &str| {
            blocks
                .iter()
                .find(|b| b.id == id)
                .unwrap_or_else(|| panic!("block {id}"))
                .parent_id
                .clone()
        };
        assert_eq!(
            parent("lnk0000000000000000001").as_deref(),
            Some("frm0000000000000000001")
        );
        assert_eq!(
            parent("crd0000000000000000001").as_deref(),
            Some("frm0000000000000000001")
        );
        assert_eq!(parent("box0000000000000000001"), None);
        assert_eq!(parent("frm0000000000000000001"), None);
        // A frame that was deleted is not a parent.
        let mut scene = v2_scene();
        scene
            .elements
            .get_mut("frm0000000000000000001")
            .unwrap()
            .insert("isDeleted".into(), Value::Bool(true));
        let blocks = blocks_of(&scene);
        assert!(blocks.iter().all(|b| b.parent_id.is_none()));
        assert!(blocks.iter().all(|b| b.id != "frm0000000000000000001"));
    }

    #[test]
    fn card_content_names_class_and_label() {
        let blocks = blocks_of(&v2_scene());
        let card = blocks
            .iter()
            .find(|b| b.id == "crd0000000000000000001")
            .expect("card block");
        assert_eq!(card.block_type, "card");
        assert_eq!(card.content, "DreamReport: The Wolf Man dream");
        // A document-linked box already says its title as its text: no repeated words.
        let linked = blocks
            .iter()
            .find(|b| b.id == "lnk0000000000000000001")
            .expect("linked box");
        assert_eq!(linked.content, "The Interpretation of Dreams");
        // A link title that adds words follows the text after an em dash.
        let mut scene = v2_scene();
        scene
            .elements
            .get_mut("box0000000000000000001")
            .unwrap()
            .insert(
                "customData".into(),
                serde_json::json!({"mnemosyne": {"kind": "document", "graphId": "g", "id": "d", "title": "Die Traumdeutung"}}),
            );
        let blocks = blocks_of(&scene);
        assert_eq!(
            blocks[0].content,
            "Freud: the dream-work \u{2014} Die Traumdeutung"
        );
    }
}
