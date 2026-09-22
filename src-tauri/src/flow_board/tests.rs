//! Unit G2 test suite — see this unit's brief §Tests. In-process: no room,
//! no store, no cluster. Red-first evidence: `tmp/mithras-flow/build/logs/G2/`.

use std::collections::BTreeSet;

use serde_json::json;
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, StateVector, Transact, TransactionMut, Update};

use super::*;
use crate::emporium::terms::Term;

/// CARGO_MANIFEST_DIR-relative (per `emporium/fixtures.rs`'s own discipline —
/// no sibling-repo path, so the crate stays portable): the byte-for-byte copy
/// of `build/contracts/golden-board.json` this unit's brief asks to place at
/// `src-tauri/tests/fixtures/flow/golden-board.json`.
const GOLDEN_BOARD_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/flow/golden-board.json"
));

const GOLDEN_SHA256: &str = "086e5c89d3a95404eb771ca4f0d36af5e60c61a897a67e3d0917d2a5d314a33a";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ===========================================================================
// board_rows fidelity to interfaces.md §B (fields board_export_json itself
// never needs to re-consult, but that must still round-trip correctly)
// ===========================================================================

#[test]
fn board_rows_captures_table_order_import_floor_and_extent_origin() {
    let doc = board_from_json(GOLDEN_BOARD_JSON).unwrap();
    let rows = board_rows(&doc);

    let expected_order: Vec<String> = columns::TABLE_ORDER
        .iter()
        .map(|(j, _, _, _)| j.to_string())
        .collect();
    assert_eq!(
        rows.table_order, expected_order,
        "resource.meta.tableOrder must be the 13 canonical names, in emptyModel() order"
    );
    assert_eq!(rows.import_floor.as_deref(), Some("imported-unknown"));

    let imu_extent = rows
        .extent
        .get("sys-imu")
        .and_then(|e| e.as_ref())
        .expect("sys-imu has an extent register");
    assert_eq!(
        imu_extent.origin, "imported-unknown",
        "the one import-time seed always tags origin"
    );
    assert_eq!(imu_extent.width, None);
    assert_eq!(imu_extent.height, None);

    let uav_extent = rows
        .extent
        .get("sys-uav")
        .and_then(|e| e.as_ref())
        .expect("sys-uav has an extent register");
    assert_eq!(uav_extent.width, Some(420.0));
    assert_eq!(uav_extent.height, Some(280.0));
}

// ===========================================================================
// Golden round trip (interfaces.md §H0, §E)
// ===========================================================================

#[test]
fn golden_fixture_bytes_and_sha_are_pinned() {
    assert_eq!(GOLDEN_BOARD_JSON.len(), 2311);
    assert_eq!(sha256_hex(GOLDEN_BOARD_JSON.as_bytes()), GOLDEN_SHA256);
}

/// The reference canonical export: `board_from_json(golden)` piped through
/// `board_export_json`. **Not** asserted byte-identical to the raw golden
/// file — see `golden_export_matches_golden_bytes_outside_the_derived_group_extents`
/// and this unit's reported deviation for why. This is the shared reference
/// several tests below compare against (scramble, determinism, update
/// round-trip): those are about INPUT-ORDERING and WIRE-ENCODING
/// insensitivity, not about re-litigating the derive itself, which
/// `golden_derive_matches_hand_verified_containment` covers directly.
fn reference_export() -> BoardExport {
    let doc = board_from_json(GOLDEN_BOARD_JSON).expect("golden fixture parses");
    board_export_json(&doc, None)
}

fn blank_system_extents(v: &mut JsonValue) {
    if let Some(systems) = v.get_mut("systems").and_then(JsonValue::as_array_mut) {
        for row in systems {
            if let Some(obj) = row.as_object_mut() {
                obj.insert("width".to_string(), JsonValue::Null);
                obj.insert("height".to_string(), JsonValue::Null);
            }
        }
    }
}

/// `board_export_json(board_from_json(golden))` is byte-identical to the
/// golden fixture for EVERY field except `systems[*].width`/`height` — see
/// the deviation this unit reports: golden's own `sys-avionics` (a group —
/// it owns `sys-imu` — with a NULL stored extent) and `sys-uav` (a group
/// whose stored 420x280 does not actually contain `sys-avionics` at its own
/// position) are exactly the cases FLOW-GARDEN-P2-amended's derive exists to
/// correct, and the amended ruling names "serialize" as one of the readers
/// that CONSUMES the derived `effectiveExtent` — its own acceptance text
/// says plainly "a containment failure, not a non-zero delta, is the
/// receipt that reopens the row." `golden_derive_matches_hand_verified_containment`
/// covers the two changed fields directly.
///
/// I2's surviving mutant, guarded in the failing direction: scrambling a
/// serializer's key order changes export BYTES while every value-level
/// assertion above stays green (`serde_json::Value` equality is
/// key-order-blind). Form C is a BYTE canon, so this guard compares the raw
/// export string against the golden fixture string with only the
/// width/height NUMBERS blanked — the P2-amended derived extents are the one
/// sanctioned delta. Proven red against the preserved I2 mutant
/// (tag `i2-mutant-scratch`, forge context 9d3113bd…): the name/description
/// column swap flips these bytes while the value-level test stays green.
#[test]
fn golden_export_matches_golden_bytes_at_string_level_outside_extents() {
    let export = reference_export();
    let blank = |s: &str| {
        regex::Regex::new(r#""(width|height)":\s*(-?[0-9]+(\.[0-9]+)?|null)"#)
            .expect("static pattern compiles")
            .replace_all(s, "\"$1\":0")
            .into_owned()
    };
    assert_eq!(
        blank(&export.json),
        blank(GOLDEN_BOARD_JSON),
        "export bytes must equal the golden fixture byte-for-byte outside width/height values \
         — form C is a byte canon; a key-order change is a real diff, not a cosmetic one"
    );
}

#[test]
fn golden_export_matches_golden_bytes_outside_the_derived_group_extents() {
    let export = reference_export();
    let mut got: JsonValue = serde_json::from_str(&export.json).expect("export is valid JSON");
    let mut want: JsonValue =
        serde_json::from_str(GOLDEN_BOARD_JSON).expect("golden is valid JSON");
    blank_system_extents(&mut got);
    blank_system_extents(&mut want);
    assert_eq!(
        got, want,
        "every field except systems[*].width/height must be byte-for-byte the golden fixture"
    );
    assert!(
        export.report.cycles.is_empty(),
        "golden fixture has no parent cycles"
    );
}

/// Hand-verified against golden's actual hierarchy: `sys-uav` (root, child
/// `sys-avionics`, stored 420x280) and `sys-avionics` (child of uav, owns
/// leaf `sys-imu` at x=10,y=20 with a 1-requirement/1-task info block, stored
/// null) are both groups. `sys-avionics`'s floor is `max(420, 10+200+20)` x
/// `max(280, 20+(44+28+30+30+12)+20)` = 420x280 (the DEFAULT_GROUP baseline
/// wins on both axes; a null stored extent + children means the floor IS the
/// emitted value). `sys-uav`'s floor is then `max(420, 40+420+20)` x
/// `max(280, 60+280+20)` = 480x360 — its own stored 420x280 does NOT contain
/// `sys-avionics`, so the derived, larger value is what's kept.
#[test]
fn golden_derive_matches_hand_verified_containment() {
    let export = reference_export();
    let mut deltas: Vec<(String, (Option<f64>, Option<f64>), (f64, f64), (f64, f64))> = export
        .report
        .group_deltas
        .iter()
        .map(|d| (d.system_id.clone(), d.stored, d.floor, d.emitted))
        .collect();
    deltas.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        deltas,
        vec![
            (
                "sys-avionics".to_string(),
                (None, None),
                (420.0, 280.0),
                (420.0, 280.0)
            ),
            (
                "sys-uav".to_string(),
                (Some(420.0), Some(280.0)),
                (480.0, 360.0),
                (480.0, 360.0)
            ),
        ]
    );
    let uav = deltas.iter().find(|(id, ..)| id == "sys-uav").unwrap();
    assert!(
        40.0 + 420.0 <= uav.3 .0,
        "sys-avionics (at x=40, width 420) must fit inside sys-uav's emitted width"
    );
    assert!(
        60.0 + 280.0 <= uav.3 .1,
        "sys-avionics (at y=60, height 280) must fit inside sys-uav's emitted height"
    );
}

#[test]
fn export_is_idempotent_on_a_second_pass() {
    let first = reference_export();
    let doc2 = board_from_json(&first.json).expect("first export re-parses");
    let second = board_export_json(&doc2, None);
    assert_eq!(
        first.json, second.json,
        "no machine-authored extent is ever written back to the model, but every reader's \
         computed value must itself be a fixed point — a second import/export pass changes nothing"
    );
}

/// Scrambled input (reversed top-level table order, reversed per-row field
/// order) → the same canonical bytes as the (derive-corrected) reference.
/// Proves row-key-order and table-order are canonicalized by the WRITER
/// (RTRIP-2), never inherited from the reader's happenstance HashMap/parse
/// order.
#[test]
fn scrambled_input_exports_identically_to_the_reference_export() {
    let scrambled = scramble_json(GOLDEN_BOARD_JSON);
    assert_ne!(
        scrambled, GOLDEN_BOARD_JSON,
        "the scramble must actually change the byte sequence, or this test proves nothing"
    );
    let doc = board_from_json(&scrambled).expect("scrambled golden fixture still parses");
    let export = board_export_json(&doc, None);
    assert_eq!(export.json, reference_export().json);
}

/// Reverses every object's key order, recursively — top-level table order
/// AND every row's field order in one pass, using only serde_json's own
/// order-preserving `Map` (`preserve_order` feature, already crate-wide) so
/// the "scramble" can never silently invent or drop a field.
fn scramble_json(text: &str) -> String {
    let v: JsonValue = serde_json::from_str(text).expect("valid JSON");
    let scrambled = reverse_object_keys(v);
    serde_json::to_string(&scrambled).expect("re-serializes")
}

fn reverse_object_keys(v: JsonValue) -> JsonValue {
    match v {
        JsonValue::Object(map) => {
            let mut reversed = serde_json::Map::new();
            for (k, val) in map.into_iter().rev() {
                reversed.insert(k, reverse_object_keys(val));
            }
            JsonValue::Object(reversed)
        }
        JsonValue::Array(items) => {
            JsonValue::Array(items.into_iter().map(reverse_object_keys).collect())
        }
        other => other,
    }
}

// ===========================================================================
// Determinism
// ===========================================================================

#[test]
fn two_docs_built_from_differently_ordered_input_export_identically() {
    let scrambled = scramble_json(GOLDEN_BOARD_JSON);
    let doc_a = board_from_json(GOLDEN_BOARD_JSON).unwrap();
    let doc_b = board_from_json(&scrambled).unwrap();
    let export_a = board_export_json(&doc_a, None);
    let export_b = board_export_json(&doc_b, None);
    assert_eq!(export_a.json, export_b.json);
}

#[test]
fn export_survives_a_yrs_update_encode_decode_round_trip() {
    let doc = board_from_json(GOLDEN_BOARD_JSON).unwrap();
    let bytes = {
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&StateVector::default())
    };

    let doc2 = Doc::new();
    {
        let mut txn2 = doc2.transact_mut();
        let update = Update::decode_v1(&bytes).expect("decodes");
        TransactionMut::apply_update(&mut txn2, update).expect("applies");
    }

    let export2 = board_export_json(&doc2, None);
    assert_eq!(export2.json, reference_export().json);
}

// ===========================================================================
// FLOW-GARDEN-P2 as amended
// ===========================================================================

fn system_row(
    id: &str,
    parent_id: Option<&str>,
    x: f64,
    y: f64,
    width: JsonValue,
    height: JsonValue,
) -> JsonValue {
    json!({
        "id": id, "parent_id": parent_id, "name": id, "description": "",
        "display_mode": "solid", "color": "#111111",
        "x": x, "y": y, "width": width, "height": height,
    })
}

fn board_json(systems: Vec<JsonValue>) -> String {
    json!({ "schemaVersion": "1", "systems": systems }).to_string()
}

#[test]
fn group_extent_derives_to_at_least_the_fit_to_content_floor() {
    // grp-1: stored 100x100, one child at (500, 300) with no stored extent.
    let text = board_json(vec![
        system_row("grp-1", None, 0.0, 0.0, json!(100.0), json!(100.0)),
        system_row(
            "child-1",
            Some("grp-1"),
            500.0,
            300.0,
            JsonValue::Null,
            JsonValue::Null,
        ),
    ]);
    let doc = board_from_json(&text).unwrap();
    let export = board_export_json(&doc, None);

    assert_eq!(
        export.report.group_deltas.len(),
        1,
        "exactly one group (grp-1)"
    );
    let delta = &export.report.group_deltas[0];
    assert_eq!(delta.system_id, "grp-1");
    assert_eq!(delta.stored, (Some(100.0), Some(100.0)));
    assert!(
        delta.emitted.0 >= delta.floor.0 && delta.emitted.1 >= delta.floor.1,
        "emitted extent {:?} must be >= the recorded floor {:?}",
        delta.emitted,
        delta.floor
    );
    // The child sits at x=500 with a leaf default width of 200 and a 20px
    // margin, so grp-1's width floor must exceed its 100-wide stored value —
    // i.e. the derive genuinely overrode the too-small stored extent.
    assert!(
        delta.floor.0 > 100.0,
        "child at x=500 must force a floor > the 100 stored width"
    );
    assert!(
        delta.emitted.0 > 100.0,
        "emitted width must reflect the floor, not the too-small stored value"
    );

    let exported_systems = exported_table(&export.json, "systems");
    let grp = exported_systems
        .iter()
        .find(|r| r["id"] == "grp-1")
        .expect("grp-1 present");
    assert_eq!(grp["width"].as_f64(), Some(delta.emitted.0));
    assert_eq!(grp["height"].as_f64(), Some(delta.emitted.1));

    let child = exported_systems
        .iter()
        .find(|r| r["id"] == "child-1")
        .expect("child-1 present");
    assert!(
        child["width"].is_null(),
        "child-1's own INPUT width was null — its export must stay null too \
         (this does not exercise EF-7, which is height-only: see \
         `leaf_real_stored_width_round_trips_unmodified` below for a leaf \
         whose input width is non-null)"
    );
    assert!(
        child["height"].is_null(),
        "EF-7: a leaf carries no stored height, regardless of input"
    );
}

#[test]
fn human_origin_extent_larger_than_the_floor_is_kept_unchanged() {
    // grp-2: stored 5000x5000 (far larger than any floor its one nearby
    // child could force), so the amended-P2 max() must keep it verbatim.
    let text = board_json(vec![
        system_row("grp-2", None, 0.0, 0.0, json!(5000.0), json!(5000.0)),
        system_row(
            "child-2",
            Some("grp-2"),
            10.0,
            10.0,
            JsonValue::Null,
            JsonValue::Null,
        ),
    ]);
    let doc = board_from_json(&text).unwrap();
    let export = board_export_json(&doc, None);

    let delta = export
        .report
        .group_deltas
        .iter()
        .find(|d| d.system_id == "grp-2")
        .expect("grp-2 delta present");
    assert_eq!(delta.stored, (Some(5000.0), Some(5000.0)));
    assert!(
        delta.floor.0 < 5000.0 && delta.floor.1 < 5000.0,
        "floor must be well under the stored value"
    );
    assert_eq!(
        delta.emitted,
        (5000.0, 5000.0),
        "the larger human-origin value is kept, not the floor"
    );
}

// ===========================================================================
// Repair 1 (G2 must-fix #1/#2): a leaf's real stored width is ordinary
// resource data, not derived geometry — FLOW-EF-7 ("Leaves carry no stored
// height") names height only. NodeResizer renders unconditionally on every
// system node (SystemNode.tsx:18-21); resizeNode sets a leaf's real `width`
// from the drag while forcing only `height` to null (useModelStore.ts
// :308-323); toFlow.ts:104-106 reads `s.width` for a leaf when non-null.
// Both call sites that were dropping it are covered separately below, since
// they are independent bugs with independent fixes (validator's own
// framing): the final-assembly loop in `derive_systems_geometry` (what gets
// EXPORTED for the leaf itself), and the leaf branch of `effective_extent`
// (what a leaf CONTRIBUTES to an ancestor group's fit-to-content floor).
// ===========================================================================

#[test]
fn leaf_real_stored_width_round_trips_unmodified() {
    // A lone leaf (no parent, no children — so `is_group` is false and only
    // `derive_systems_geometry`'s final-assembly loop is in play, never
    // `effective_extent`'s group branch) with a real, human-resized width.
    // RTRIP-2: canonicalize "changes no field values" — a leaf's width is
    // ordinary resource data and must survive export unchanged.
    let text = board_json(vec![system_row(
        "leaf-1",
        None,
        0.0,
        0.0,
        json!(350.0),
        JsonValue::Null,
    )]);
    let doc = board_from_json(&text).unwrap();
    let export = board_export_json(&doc, None);

    assert!(
        export.report.group_deltas.is_empty(),
        "a lone leaf with no children is never a group — no delta to record"
    );

    let exported_systems = exported_table(&export.json, "systems");
    let leaf = exported_systems
        .iter()
        .find(|r| r["id"] == "leaf-1")
        .expect("leaf-1 present");
    assert_eq!(
        leaf["width"].as_f64(),
        Some(350.0),
        "a leaf's real stored width must round-trip, not be discarded to null"
    );
    assert!(
        leaf["height"].is_null(),
        "EF-7 still forces height to null for a leaf, input null or not"
    );
}

#[test]
fn a_resized_leafs_real_width_is_the_footprint_used_for_its_parents_floor() {
    // grp-w has no stored extent; its one child leaf-w has been resized to a
    // REAL stored width of 500 — far wider than the 200px per-kind default
    // `effective_extent` falls back to only when nothing was ever stored
    // (fitAncestors' `cur.width ?? 200`, useModelStore.ts:190). Positioned
    // at x=250, only the leaf's true 500px width pushes the parent's floor
    // to 770 (250 + 500 + FIT_MARGIN 20). If the leaf's stored width were
    // ignored (the bug this test guards against), the footprint would fall
    // back to 200 and the floor would land at 470 (250 + 200 + 20) instead —
    // still > DEFAULT_GROUP_WIDTH (420), so the two cases are unambiguously
    // distinguishable, not just both clipped to the same group default.
    let text = board_json(vec![
        system_row("grp-w", None, 0.0, 0.0, JsonValue::Null, JsonValue::Null),
        system_row(
            "leaf-w",
            Some("grp-w"),
            250.0,
            0.0,
            json!(500.0),
            JsonValue::Null,
        ),
    ]);
    let doc = board_from_json(&text).unwrap();
    let export = board_export_json(&doc, None);

    let delta = export
        .report
        .group_deltas
        .iter()
        .find(|d| d.system_id == "grp-w")
        .expect("grp-w delta present");
    assert_eq!(
        delta.stored,
        (None, None),
        "grp-w was never given a stored extent"
    );
    assert_eq!(
        delta.floor.0, 770.0,
        "floor must be computed from the leaf's REAL 500px stored width \
         (250 + 500 + 20 margin) — the 200px-fallback bug would floor at \
         470 instead"
    );
    assert_eq!(
        delta.emitted.0, 770.0,
        "no stored extent on grp-w to max() against — emitted equals floor"
    );

    let exported_systems = exported_table(&export.json, "systems");
    let leaf = exported_systems
        .iter()
        .find(|r| r["id"] == "leaf-w")
        .expect("leaf-w present");
    assert_eq!(
        leaf["width"].as_f64(),
        Some(500.0),
        "leaf-w's own real stored width must also round-trip unmodified"
    );
}

// ===========================================================================
// D3 — parent cycles: render-only repair as a Proposal, model never written
// ===========================================================================

#[test]
fn two_cycle_yields_one_cycle_proposal_and_every_row_still_exports() {
    // sys-b > sys-a lexicographically, so sys-b is the detached member.
    let text = board_json(vec![
        system_row(
            "sys-a",
            Some("sys-b"),
            0.0,
            0.0,
            JsonValue::Null,
            JsonValue::Null,
        ),
        system_row(
            "sys-b",
            Some("sys-a"),
            0.0,
            0.0,
            JsonValue::Null,
            JsonValue::Null,
        ),
    ]);
    let doc = board_from_json(&text).unwrap();
    let export = board_export_json(&doc, None);

    assert_eq!(
        export.report.cycles.len(),
        1,
        "exactly one CycleProposal for one two-cycle"
    );
    let cycle = &export.report.cycles[0];
    assert_eq!(cycle.cycle, vec!["sys-a".to_string(), "sys-b".to_string()]);
    assert_eq!(cycle.suggested_detach, "sys-b");

    // "the model is never machine-written": both rows still export with
    // their ORIGINAL, untouched parent_id — the repair is derive-view-only.
    let exported_systems = exported_table(&export.json, "systems");
    assert_eq!(
        exported_systems.len(),
        2,
        "both cyclic rows are still exported"
    );
    let a = exported_systems
        .iter()
        .find(|r| r["id"] == "sys-a")
        .unwrap();
    let b = exported_systems
        .iter()
        .find(|r| r["id"] == "sys-b")
        .unwrap();
    assert_eq!(
        a["parent_id"],
        json!("sys-b"),
        "sys-a's parent_id is untouched by the repair"
    );
    assert_eq!(
        b["parent_id"],
        json!("sys-a"),
        "sys-b's parent_id is untouched by the repair"
    );
}

// ===========================================================================
// board_desired_triples (interfaces.md §G)
// ===========================================================================

fn golden_rows() -> BoardRows {
    let doc = board_from_json(GOLDEN_BOARD_JSON).unwrap();
    board_rows(&doc)
}

fn find_triple<'a>(
    triples: &'a [Triple],
    subject: &str,
    predicate_local: &str,
) -> Option<&'a Term> {
    let predicate = format!("{FLOW_NS}{predicate_local}");
    triples
        .iter()
        .find(|(s, p, _)| s == subject && *p == predicate)
        .map(|(_, _, t)| t)
}

#[test]
fn subject_count_per_class_equals_row_count() {
    let rows = golden_rows();
    let gs = crate::rdf::graph_subject("g2-test-graph");
    let triples = board_desired_triples(&rows, &gs);

    for (_, ddl_table, _, pascal) in columns::TABLE_ORDER {
        let row_count = rows
            .tables
            .get(ddl_table)
            .map(|t| t.rows.len())
            .unwrap_or(0);
        let class_iri = format!("{FLOW_NS}{pascal}");
        let subjects: BTreeSet<&str> = triples
            .iter()
            .filter(|(_, p, o)| p == RDF_TYPE_URI && o.as_nt() == format!("<{class_iri}>"))
            .map(|(s, _, _)| s.as_str())
            .collect();
        assert_eq!(
            subjects.len(),
            row_count,
            "{ddl_table} ({pascal}): {row_count} rows but {} typed subjects",
            subjects.len()
        );
    }
}

#[test]
fn five_predicate_spot_checks_against_the_vocabulary_map() {
    let rows = golden_rows();
    let gs = crate::rdf::graph_subject("g2-test-graph");
    let triples = board_desired_triples(&rows, &gs);

    let req1 = format!("{gs}:projection:flow:requirement:req-1");
    let sys_imu = format!("{gs}:projection:flow:system:sys-imu");

    // 1. Requirement.ownedBy is a class-qualified URI (system_id -> System).
    let owned_by = find_triple(&triples, &req1, "ownedBy").expect("flow:ownedBy present on req-1");
    assert_eq!(owned_by.as_nt(), format!("<{sys_imu}>"));

    // 2. System.displayMode is present (string).
    let display_mode = find_triple(&triples, &sys_imu, "displayMode")
        .expect("flow:displayMode present on sys-imu");
    assert_eq!(display_mode.as_nt(), "\"solid\"");

    // 3. Requirement.sortOrder is xsd:integer.
    let sort_order =
        find_triple(&triples, &req1, "sortOrder").expect("flow:sortOrder present on req-1");
    assert_eq!(
        sort_order.as_nt(),
        "\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    );

    // 4. Task.done is xsd:boolean.
    let task1 = format!("{gs}:projection:flow:task:task-1");
    let done = find_triple(&triples, &task1, "done").expect("flow:done present on task-1");
    assert_eq!(
        done.as_nt(),
        "\"false\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
    );

    // 5. Trade.tradeRationale is the RTRIP-11 #3 rename (trades.description).
    let trd1 = format!("{gs}:projection:flow:trade:trd-1");
    let rationale = find_triple(&triples, &trd1, "tradeRationale")
        .expect("flow:tradeRationale present on trd-1");
    assert_eq!(rationale.as_nt(), "\"picked on bias\"");
}

#[test]
fn display_mode_predicate_is_present_and_no_geometry_predicates_exist() {
    let rows = golden_rows();
    let gs = crate::rdf::graph_subject("g2-test-graph");
    let triples = board_desired_triples(&rows, &gs);

    let display_mode_pred = format!("{FLOW_NS}displayMode");
    assert!(
        triples.iter().any(|(_, p, _)| p == &display_mode_pred),
        "at least one flow:displayMode triple must exist"
    );

    let x_pred = format!("{FLOW_NS}x");
    let y_pred = format!("{FLOW_NS}y");
    let waypoints_pred = format!("{FLOW_NS}waypoints");
    let width_pred = format!("{FLOW_NS}width");
    let height_pred = format!("{FLOW_NS}height");
    for (_, p, _) in &triples {
        assert_ne!(
            p, &x_pred,
            "no flow:x predicate may exist (geometry, interfaces.md §C)"
        );
        assert_ne!(p, &y_pred, "no flow:y predicate may exist (geometry)");
        assert_ne!(
            p, &waypoints_pred,
            "no flow:waypoints predicate may exist (geometry)"
        );
        assert_ne!(
            p, &width_pred,
            "no flow:width predicate may exist (geometry)"
        );
        assert_ne!(
            p, &height_pred,
            "no flow:height predicate may exist (geometry)"
        );
    }
}

#[test]
fn triples_are_sorted_and_flow_ns_matches_g1s_registered_pack() {
    let rows = golden_rows();
    let gs = crate::rdf::graph_subject("g2-test-graph");
    let triples = board_desired_triples(&rows, &gs);
    assert!(!triples.is_empty());
    let mut sorted = triples.clone();
    sorted.sort_by(|a, b| {
        (a.0.as_str(), a.1.as_str(), a.2.as_nt()).cmp(&(b.0.as_str(), b.1.as_str(), b.2.as_nt()))
    });
    let orig: Vec<(String, String, String)> = triples
        .iter()
        .map(|(s, p, o)| (s.clone(), p.clone(), o.as_nt()))
        .collect();
    let sorted_view: Vec<(String, String, String)> = sorted
        .iter()
        .map(|(s, p, o)| (s.clone(), p.clone(), o.as_nt()))
        .collect();
    assert_eq!(
        orig, sorted_view,
        "board_desired_triples must already return sorted output"
    );

    let contract =
        crate::emporium::contract::get_vocabulary("flow").expect("G1's flow pack is registered");
    assert_eq!(
        contract.primary_namespace(),
        FLOW_NS,
        "FLOW_NS must never drift from G1's registered flow.golden.json namespace"
    );
}

fn exported_table(json_text: &str, key: &str) -> Vec<JsonValue> {
    let v: JsonValue = serde_json::from_str(json_text).expect("export is valid JSON");
    v[key].as_array().cloned().unwrap_or_default()
}

// ===========================================================================
// item 3 — JSON number/escape parity with JS `JSON.stringify`
// ===========================================================================

#[test]
fn js_number_string_matches_js_json_stringify() {
    // Verified against a real ECMAScript engine's semantics (Number::toString
    // / JSON.stringify are spec-defined, not implementation-specific): 1 ->
    // "1"; 1.5 -> "1.5"; 0.1 -> "0.1" (shortest round-trip, not the exact
    // binary expansion); -0 -> "0" (JSON.stringify(-0) === "0", the single
    // most commonly-missed case); 1e21 -> "1e+21" (JS's exponential-notation
    // threshold, with an explicit '+' on the exponent Rust's `{:e}` omits).
    assert_eq!(js_number_string(1.0), "1");
    assert_eq!(js_number_string(1.5), "1.5");
    assert_eq!(js_number_string(0.1), "0.1");
    assert_eq!(js_number_string(-0.0), "0");
    assert_eq!(js_number_string(1e21), "1e+21");
    // A few more, to pin the exponential branch beyond the one named case.
    assert_eq!(js_number_string(-1.5), "-1.5");
    assert_eq!(js_number_string(1e-7), "1e-7");
    assert_eq!(js_number_string(420.0), "420");
    assert_eq!(js_number_string(-300.0), "-300");
}

#[test]
fn json_string_escaping_matches_js_json_stringify() {
    let mut out = String::new();
    write_json_string_value(&mut out, "a/b");
    assert_eq!(out, "\"a/b\"", "JS JSON.stringify does not escape '/'");

    let mut out = String::new();
    write_json_string_value(&mut out, "caf\u{e9}");
    assert_eq!(
        out, "\"caf\u{e9}\"",
        "non-ASCII passes through raw, unescaped"
    );

    let mut out = String::new();
    write_json_string_value(&mut out, "a\"b\\c\nd\te");
    assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\"");

    let mut out = String::new();
    write_json_string_value(&mut out, "\u{1}");
    assert_eq!(
        out, "\"\\u0001\"",
        "a non-mnemonic control char gets the uniform \\u00XX escape"
    );
}

// ===========================================================================
// Undeclared tables dropped vs. extras preserved verbatim (§E / §B)
// ===========================================================================

#[test]
fn undeclared_array_table_is_dropped_and_unknown_scalar_key_survives_in_extras() {
    let text = json!({
        "schemaVersion": "1",
        "systems": [],
        "futureTable": [{"id": "x"}],
        "chronicle": {"note": "sidecar metadata, not a table"},
    })
    .to_string();
    let doc = board_from_json(&text).unwrap();
    let export = board_export_json(&doc, None);
    let v: JsonValue = serde_json::from_str(&export.json).unwrap();
    assert!(
        v.get("futureTable").is_none(),
        "an undeclared array-shaped key is dropped, not carried"
    );
    assert_eq!(
        v.get("chronicle"),
        Some(&json!({"note": "sidecar metadata, not a table"})),
        "a genuinely unknown non-array top-level key survives in extras, verbatim"
    );
}

// ===========================================================================
// I1 differential evidence (integration seat, print-only)
// ===========================================================================

/// I1 differential (a): prints — base64-wrapped, between BEGIN/END markers,
/// with sha256 lines — the reference export of the golden fixture, the
/// scrambled input bytes, and the scrambled input's export, so the
/// integration seat can byte-compare the Rust writer's canonical form
/// against the fork's TS `canonicalize` on the SAME input bytes. The one
/// assertion here (scrambled export == golden export) duplicates
/// `scrambled_input_exports_identically_to_the_reference_export` so the
/// printed evidence can never come from two silently different code paths.
/// Run with `--nocapture` to capture the blocks.
#[test]
fn i1_differential_prints_reference_export_evidence() {
    fn print_b64_block(label: &str, bytes: &[u8]) {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        println!("{label}-B64-BEGIN");
        for chunk in encoded.as_bytes().chunks(76) {
            println!("{}", std::str::from_utf8(chunk).expect("base64 is ASCII"));
        }
        println!("{label}-B64-END");
    }

    let golden_export = reference_export();
    println!(
        "I1-GOLDEN-EXPORT-SHA256 {}",
        sha256_hex(golden_export.json.as_bytes())
    );
    print_b64_block("I1-GOLDEN-EXPORT", golden_export.json.as_bytes());

    let scrambled = scramble_json(GOLDEN_BOARD_JSON);
    println!("I1-SCRAMBLED-INPUT-SHA256 {}", sha256_hex(scrambled.as_bytes()));
    print_b64_block("I1-SCRAMBLED-INPUT", scrambled.as_bytes());

    let doc = board_from_json(&scrambled).expect("scrambled golden fixture still parses");
    let scrambled_export = board_export_json(&doc, None);
    println!(
        "I1-SCRAMBLED-EXPORT-SHA256 {}",
        sha256_hex(scrambled_export.json.as_bytes())
    );
    assert_eq!(scrambled_export.json, golden_export.json);
}
