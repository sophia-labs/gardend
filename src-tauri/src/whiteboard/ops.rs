//! Y.Doc reads and writes for the whiteboard (FORMAT.md): the `elements`
//! `Y.Map<string,string>`, one JSON string per Excalidraw element, and `meta`.

use super::scene::{Element, Scene};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use yrs::{
    Any, Doc, Map as YMap, MapRef, Out, ReadTxn, StateVector, Transact, TransactionMut, WriteTxn,
};

pub(crate) const SCHEMA: &str = "sophia.whiteboard/1";

/// Read every element (live and tombstoned) of a Y.Doc. An entry whose JSON does
/// not parse is skipped: a peer's malformed write must not take a reader down.
pub(crate) fn read_scene<T: ReadTxn>(txn: &T) -> Scene {
    let mut elements: BTreeMap<String, Element> = BTreeMap::new();
    if let Some(map) = txn.get_map("elements") {
        for (key, value) in map.iter(txn) {
            let text = match value {
                Out::Any(Any::String(text)) => text.to_string(),
                _ => continue,
            };
            if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&text) {
                elements.insert(key.to_string(), object);
            }
        }
    }
    Scene::from_elements(elements)
}

pub(crate) fn scene_of_doc(doc: &Doc) -> Scene {
    let txn = doc.transact();
    read_scene(&txn)
}

pub(crate) fn scene_from_update_bytes(bytes: &[u8]) -> Result<Scene, String> {
    use yrs::updates::decoder::Decode as _;
    let doc = Doc::new();
    if !bytes.is_empty() {
        let update = yrs::Update::decode_v1(bytes)
            .map_err(|error| format!("decode whiteboard Y.Doc update: {error}"))?;
        let mut txn = doc.transact_mut();
        txn.apply_update(update)
            .map_err(|error| format!("apply whiteboard Y.Doc update: {error}"))?;
    }
    Ok(scene_of_doc(&doc))
}

pub(crate) fn scene_from_update_base64(encoded: &str) -> Result<Scene, String> {
    use base64::Engine as _;
    if encoded.is_empty() {
        return Ok(Scene::default());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| format!("decode whiteboard Y.Doc base64: {error}"))?;
    scene_from_update_bytes(&bytes)
}

/// Write the scene's dirty elements, and `meta` if absent, into the transaction.
pub(crate) fn write_dirty(txn: &mut TransactionMut<'_>, scene: &Scene) {
    let elements = txn.get_or_insert_map("elements");
    for id in &scene.dirty {
        if let Some(el) = scene.elements.get(id) {
            if let Ok(text) = serde_json::to_string(el) {
                elements.insert(txn, id.as_str(), text);
            }
        }
    }
    ensure_meta(txn);
}

pub(crate) fn ensure_meta(txn: &mut TransactionMut<'_>) {
    let meta: MapRef = txn.get_or_insert_map("meta");
    if meta.get(txn, "schema").is_none() {
        meta.insert(txn, "schema", SCHEMA);
    }
    if meta.get(txn, "viewBackgroundColor").is_none() {
        meta.insert(txn, "viewBackgroundColor", "#ffffff");
    }
}

/// A cheap monotonic-in-practice token: changes whenever the doc's state does.
/// The hash of the state vector, which names every client's clock.
pub(crate) fn revision_token(doc: &Doc) -> String {
    let txn = doc.transact();
    let sv: StateVector = txn.state_vector();
    use yrs::updates::encoder::Encode as _;
    let digest = Sha256::digest(sv.encode_v1());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}
