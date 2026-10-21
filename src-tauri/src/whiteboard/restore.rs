//! Forward restore of a whiteboard (format sophia.whiteboard/1, whiteboard/FORMAT.md).
//!
//! Why not a plain map restore: the board's elements are whole-element JSON strings, and Excalidraw's own
//! reconcile keeps the copy with the higher `version` (lower `versionNonce` on a tie). A restored element
//! carrying its old version would lose to the newer copy every open browser still holds, so the restore
//! would silently not show. Every element the restore writes therefore gets a version above both sides, and
//! a deletion is a tombstone (`isDeleted: true` with a bumped version), never a removed key.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use yrs::{Doc, Map, Transact, TransactionMut, WriteTxn};

use super::ops::{read_scene, scene_of_doc};
use super::scene::{is_deleted, n, Element};
use crate::crdt_engine::forward_restore::RestoreContext;
use crate::crdt_engine::minimal_edit::{ytree, OpCounts};

/// Roots restored as plain maps: the board's `meta` and the (v1-unused) `files`.
const PLAIN_ROOTS: &[&str] = &["meta", "files"];

/// An element without the fields every write changes: `version`, `versionNonce`, `updated` and the
/// `customData.sophia` attribution stamp. Two elements equal in this form are the same drawing.
fn comparable(el: &Element) -> Element {
    let mut out = el.clone();
    for key in ["version", "versionNonce", "updated"] {
        out.remove(key);
    }
    let drop_custom_data = match out.get_mut("customData") {
        Some(Value::Object(custom)) => {
            custom.remove("sophia");
            custom.is_empty()
        }
        Some(Value::Null) => true,
        _ => false,
    };
    if drop_custom_data {
        out.remove("customData");
    }
    out
}

/// Equal enough to leave alone: the same drawing, or both tombstones (what a deleted element held is moot).
fn equivalent(have: &Element, want: &Element) -> bool {
    (is_deleted(have) && is_deleted(want)) || comparable(have) == comparable(want)
}

/// A fresh `versionNonce`, derived from the operation so a retried restore stamps the same value.
fn nonce(ctx: &RestoreContext, id: &str, version: i64) -> i64 {
    let digest = Sha256::digest(
        format!(
            "{}|{}|{}|{}",
            ctx.operation_id, ctx.restore_point_id, id, version
        )
        .as_bytes(),
    );
    let word = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    i64::from(word % 2_000_000_000)
}

/// `el` stamped as a restore write at `version`: fresh nonce, `updated`, and `customData.sophia`.
fn restamped(ctx: &RestoreContext, id: &str, mut el: Element, version: i64) -> Element {
    let now = ctx.now_ms as i64;
    el.insert("version".to_string(), json!(version));
    el.insert("versionNonce".to_string(), json!(nonce(ctx, id, version)));
    el.insert("updated".to_string(), json!(now));
    let mut custom = match el.remove("customData") {
        Some(Value::Object(custom)) => custom,
        _ => Element::new(),
    };
    custom.insert("sophia".to_string(), json!({"by": "restore", "at": now}));
    el.insert("customData".to_string(), Value::Object(custom));
    el
}

/// Per element id: equal to the target modulo `version`/`versionNonce`/`updated`/`customData.sophia` ->
/// untouched; otherwise write the target with `version = max(current, target) + 1`, a fresh `versionNonce`,
/// `updated = ctx.now_ms`, `customData.sophia = {by: "restore", at}`; present now but absent in the target ->
/// tombstone (`isDeleted: true`, version bumped). Excalidraw's reconcile (higher version wins) then accepts
/// the restore in every open browser. `meta` and `files` through `ytree::restore_map_roots`.
pub(crate) fn restore_elements(
    txn: &mut TransactionMut<'_>,
    target: &Doc,
    ctx: &RestoreContext,
) -> Result<OpCounts, String> {
    let current = read_scene(&*txn);
    let wanted = scene_of_doc(target);

    // Decide every write first (pure), then apply them: BTreeMap order keeps the operations deterministic.
    let mut writes: Vec<(String, Element)> = Vec::new();
    for (id, want) in &wanted.elements {
        match current.elements.get(id) {
            Some(have) => {
                if equivalent(have, want) {
                    continue;
                }
                let version = n(have, "version").max(n(want, "version")) as i64 + 1;
                writes.push((id.clone(), restamped(ctx, id, want.clone(), version)));
            }
            None => {
                // Absent now and a tombstone then: the same board.
                if is_deleted(want) {
                    continue;
                }
                let version = n(want, "version") as i64 + 1;
                writes.push((id.clone(), restamped(ctx, id, want.clone(), version)));
            }
        }
    }
    for (id, have) in &current.elements {
        if wanted.elements.contains_key(id) || is_deleted(have) {
            continue;
        }
        let mut tombstone = have.clone();
        tombstone.insert("isDeleted".to_string(), Value::Bool(true));
        let version = n(have, "version") as i64 + 1;
        writes.push((id.clone(), restamped(ctx, id, tombstone, version)));
    }

    let mut counts = OpCounts::default();
    if !writes.is_empty() {
        let elements = txn.get_or_insert_map("elements");
        for (id, el) in &writes {
            let text = serde_json::to_string(el)
                .map_err(|error| format!("whiteboard restore: serialize element {id}: {error}"))?;
            elements.insert(txn, id.as_str(), text);
            counts.map_keys_set += 1;
        }
    }

    let target_txn = target.transact();
    counts.add(&ytree::restore_map_roots(txn, PLAIN_ROOTS, &target_txn));
    Ok(counts)
}

/// Live elements equal modulo version, versionNonce, updated and customData.sophia; tombstones ignored.
pub(crate) fn scenes_equivalent(a: &Doc, b: &Doc) -> bool {
    let live = |doc: &Doc| -> BTreeMap<String, Element> {
        scene_of_doc(doc)
            .live()
            .map(|(id, el)| (id.clone(), comparable(el)))
            .collect()
    };
    live(a) == live(b)
}
