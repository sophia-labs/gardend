//! `flow_board` — the deterministic Mithras Flow board projection and export:
//! pure functions over a `yrs::Doc`, no room, no store, no cluster (unit G2,
//! Mithras Flow playground build, lane G, after G1).
//!
//! Implements `interfaces.md` §B (the board Y.Doc shape), §C (the geometry
//! partition), §E (the export/canonical-form byte-exactness rules), §G (the
//! vocabulary map G1 wrote), and `contracts/garden.md`'s FLOW-GARDEN-P2 as
//! amended (§4: `effectiveExtent = max(stored ?? per-kind default,
//! fit(children))`, recomputed at derive, children-before-parents) and the
//! D3 parent-cycle ruling (§5: render-only repair as a Proposal, the model
//! never machine-written).
//!
//! Three entry points:
//! - [`board_from_json`] — form N (or any) Flow JSON → a fresh `yrs::Doc`
//!   materializing §B exactly.
//! - [`board_rows`] / [`board_export_json`] — sorted, deterministic reads of
//!   both roots; the latter re-joins `scene` into canonical form C (§E) plus
//!   the FLOW-GARDEN-P2 derive, returning the JSON text *and* a
//!   [`DeriveReport`] (per-group extent deltas, parent-cycle proposals).
//! - [`board_desired_triples`] — the `resource → flow:` projection triples
//!   (interfaces.md §G / `contracts/vocabulary-map.md`), consumable directly
//!   by [`crate::emporium::terms::diff_triples`]/`render_updates`.
//!
//! Byte-exactness (item 3 of this unit's brief) needs JS `JSON.stringify`
//! number/string formatting, which neither `serde_json`'s nor Rust's own
//! `Display` reproduce in every case (see [`js_number_string`]'s doc comment)
//! — so canonical-form output is hand-written (`write_*` below), never
//! `serde_json::to_string` on the final `Value`. `serde_json::Value` is still
//! used freely as an in-memory scalar carrier (parsing input, and as
//! [`TableRows`]'s field type) since only the *final byte output* needs
//! JS parity, not every intermediate representation.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;
use yrs::{
    Any, Array as YArray, ArrayPrelim, Doc, Map as YMap, MapPrelim, Out, ReadTxn, Transact,
    TransactionMut, WriteTxn,
};

use crate::emporium::contract::Datatype;
use crate::emporium::terms::{term_for, Term, Triple, Value};

mod columns;
#[cfg(test)]
mod tests;
mod vocab;

/// `flow:` = `urn:sophia:flow:vocab:` (interfaces.md §A). Hardcoded rather
/// than read from G1's registered pack so this module stays a genuinely pure,
/// dependency-free function of its inputs (no Emporium registry lookup) —
/// `tests::triples_are_sorted_and_flow_ns_matches_g1s_registered_pack`
/// cross-checks the two never drift apart.
pub(crate) const FLOW_NS: &str = "urn:sophia:flow:vocab:";

const RDF_TYPE_URI: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The board document id is the constant `flow-board` (interfaces.md §A).
pub(crate) const BOARD_DOCUMENT_ID: &str = "flow-board";

/// The workspace `documents` entry discriminator (interfaces.md §A):
/// `documentKind: "flow-board"`. Unit G3 branches the one TipTap seam
/// (`materialize_room_document`) on this value.
pub(crate) const FLOW_BOARD_KIND: &str = "flow-board";

/// The 13-table order (unit G4: the seed stats key their per-table row
/// counts by the file's own JSON spelling). Each row is
/// `(json_key, ddl_table, kebab_class, pascal_class)` — see
/// [`columns::TABLE_ORDER`].
pub(crate) fn table_order() -> &'static [(&'static str, &'static str, &'static str, &'static str)]
{
    &columns::TABLE_ORDER
}

// ===========================================================================
// Errors
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlowError(pub(crate) String);

impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlowError {}

// ===========================================================================
// board_from_json — Flow JSON (form N, or any) → a fresh Y.Doc (§B)
// ===========================================================================

/// Parse a Flow board JSON file and materialize interfaces.md §B exactly:
/// `resource.meta` (`schemaVersion` copied verbatim, defaulting to `"1"` if
/// absent; `tableOrder` = the 13 canonical names, a fixed constant — NOT
/// derived from this file's own key order, which is exactly what makes the
/// "scrambled table order" round-trip case degenerate to the golden bytes;
/// `extras` = every top-level key that is neither `schemaVersion` nor one of
/// the 13 table names, values verbatim); per-table `order` (the file's own
/// row array order, authored — RTRIP-4) and `rows` (every non-geometry field,
/// `id` included, scalars exactly as given); `scene.placement`/`extent`
/// (systems only)/`waypoints` from the geometry fields, `extent.origin =
/// "imported-unknown"`; `scene.meta.importFloor = "imported-unknown"`.
///
/// A top-level key that is NOT one of the 13 canonical table names is an
/// "undeclared table" only when its value is itself a JSON array (something
/// that structurally COULD be a row set from a future/older `TABLES` list) —
/// those are dropped per interfaces.md §E ("undeclared tables dropped (N)");
/// any other unrecognized key is genuine sidecar metadata and survives in
/// `extras`. This disambiguation rule is this unit's own reading — neither
/// interfaces.md nor the brief states a mechanism — see the unit's reported
/// deviation.
///
/// Missing tables default to empty (`order: []`, no rows). A row missing a
/// string `"id"` is unrepresentable (it cannot key `rows`/`order`) and is
/// skipped — the one input shape this function treats as truly malformed
/// rather than defaulting through.
pub(crate) fn board_from_json(text: &str) -> Result<Doc, FlowError> {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        seed_board_txn(&mut txn, text)?;
    }
    Ok(doc)
}

/// The transaction-level body of [`board_from_json`], split out (unit G4) so
/// the `flow.seed` CRDT op can seed a LIVE room inside the room's own single
/// transaction — grafting a separately-built `Doc` in via `apply_update`
/// would merge two independent Y histories and let the OLD state win the
/// per-key arbitration; writing through the room's transaction makes the
/// seed causally after everything it replaces. Every fallible step (parse,
/// root-shape check) happens BEFORE the first write, so an `Err` never
/// leaves a half-seeded transaction behind.
pub(crate) fn seed_board_txn(txn: &mut TransactionMut<'_>, text: &str) -> Result<(), FlowError> {
    let parsed: JsonValue = serde_json::from_str(text)
        .map_err(|e| FlowError(format!("invalid Flow board JSON: {e}")))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| FlowError("Flow board JSON root must be an object".to_string()))?;

    let schema_version = obj
        .get("schemaVersion")
        .and_then(JsonValue::as_str)
        .unwrap_or("1")
        .to_string();

    let known_json_keys: Vec<&str> = columns::TABLE_ORDER.iter().map(|(j, _, _, _)| *j).collect();
    let mut extras = serde_json::Map::new();
    for (k, v) in obj.iter() {
        if k == "schemaVersion" || known_json_keys.contains(&k.as_str()) {
            continue;
        }
        if v.is_array() {
            continue; // undeclared table (N): dropped, not carried as an extra
        }
        extras.insert(k.clone(), v.clone());
    }
    let extras_json =
        serde_json::to_string(&JsonValue::Object(extras)).unwrap_or_else(|_| "{}".to_string());
    let table_order_json =
        serde_json::to_string(&known_json_keys).unwrap_or_else(|_| "[]".to_string());

    let resource = txn.get_or_insert_map("resource");
    let scene = txn.get_or_insert_map("scene");

    let meta = resource.insert(&mut *txn, "meta", MapPrelim::default());
    meta.insert(&mut *txn, "schemaVersion", schema_version.as_str());
    meta.insert(&mut *txn, "tableOrder", table_order_json.as_str());
    meta.insert(&mut *txn, "extras", extras_json.as_str());

    let scene_meta = scene.insert(&mut *txn, "meta", MapPrelim::default());
    scene_meta.insert(&mut *txn, "importFloor", "imported-unknown");

    let placement = scene.insert(&mut *txn, "placement", MapPrelim::default());
    let extent = scene.insert(&mut *txn, "extent", MapPrelim::default());
    let waypoints = scene.insert(&mut *txn, "waypoints", MapPrelim::default());

    for (json_key, ddl_table, _, _) in columns::TABLE_ORDER {
        let table_map = resource.insert(&mut *txn, ddl_table, MapPrelim::default());
        let order_arr = table_map.insert(&mut *txn, "order", ArrayPrelim::default());
        let rows_map = table_map.insert(&mut *txn, "rows", MapPrelim::default());

        let Some(input_rows) = obj.get(json_key).and_then(JsonValue::as_array) else {
            continue;
        };
        for row_val in input_rows {
            let Some(row_obj) = row_val.as_object() else {
                continue;
            };
            let Some(id) = row_obj.get("id").and_then(JsonValue::as_str) else {
                continue;
            };

            let fields: Vec<(String, Any)> = row_obj
                .iter()
                .filter(|(k, _)| !columns::is_geometry_column(k))
                .map(|(k, v)| (k.clone(), json_scalar_to_any(v)))
                .collect();
            order_arr.push_back(&mut *txn, id);
            rows_map.insert(&mut *txn, id, MapPrelim::from_iter(fields));

            match ddl_table {
                "systems" => {
                    let placement_str = write_placement_json(row_obj);
                    placement.insert(&mut *txn, id, placement_str.as_str());
                    let extent_str = write_extent_json(row_obj);
                    extent.insert(&mut *txn, id, extent_str.as_str());
                }
                "workflows" | "trades" | "constraints" => {
                    let placement_str = write_placement_json(row_obj);
                    placement.insert(&mut *txn, id, placement_str.as_str());
                }
                "workflow_links" | "trade_links" | "constraint_links" | "edges" => {
                    if let Some(w) = row_obj.get("waypoints").and_then(JsonValue::as_str) {
                        waypoints.insert(&mut *txn, id, w);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Internal bookkeeping blob only (never directly compared against golden
/// bytes — re-parsed and reformatted through [`js_number_string`] at export
/// time), so plain `serde_json` formatting is fine here. Key order (`x`
/// before `y`) matches interfaces.md §B's literal shape for readability of a
/// raw dump, though it isn't load-bearing.
fn write_placement_json(row_obj: &serde_json::Map<String, JsonValue>) -> String {
    let x = row_obj.get("x").cloned().unwrap_or(JsonValue::Null);
    let y = row_obj.get("y").cloned().unwrap_or(JsonValue::Null);
    serde_json::to_string(&serde_json::json!({"x": x, "y": y})).unwrap_or_else(|_| "{}".to_string())
}

/// See [`write_placement_json`] — same internal-blob rationale. Always
/// `origin: "imported-unknown"`: `board_from_json` is the one import-time
/// seed FLOW-GARDEN-P2-amended names as a legitimate extent writer.
fn write_extent_json(row_obj: &serde_json::Map<String, JsonValue>) -> String {
    let width = row_obj.get("width").cloned().unwrap_or(JsonValue::Null);
    let height = row_obj.get("height").cloned().unwrap_or(JsonValue::Null);
    serde_json::to_string(
        &serde_json::json!({"width": width, "height": height, "origin": "imported-unknown"}),
    )
    .unwrap_or_else(|_| "{}".to_string())
}

fn json_scalar_to_any(v: &JsonValue) -> Any {
    match v {
        JsonValue::Null => Any::Null,
        JsonValue::Bool(b) => Any::Bool(*b),
        JsonValue::Number(n) => Any::Number(n.as_f64().unwrap_or(0.0)),
        JsonValue::String(s) => Any::from(s.as_str()),
        // DDL columns are always scalar; Array/Object here would mean a
        // malformed row. Defensive fallback, not expected to be hit.
        JsonValue::Array(_) | JsonValue::Object(_) => Any::Null,
    }
}

fn any_to_json_scalar(any: &Any) -> JsonValue {
    match any {
        Any::Null | Any::Undefined => JsonValue::Null,
        Any::Bool(b) => JsonValue::Bool(*b),
        Any::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Any::String(s) => JsonValue::String(s.to_string()),
        // Never written by board_from_json; defensive fallback for a doc
        // populated some other way.
        _ => JsonValue::Null,
    }
}

// ===========================================================================
// board_rows — sorted, deterministic read of both roots
// ===========================================================================

#[derive(Debug, Clone, Default)]
pub(crate) struct TableRows {
    /// Row ids in authored order (the `order` Y.Array — RTRIP-4: the file is
    /// the authority; never sorted, this genuinely is a sequence).
    pub(crate) order: Vec<String>,
    /// rowId -> field -> value. A `BTreeMap` (not the Y.Map's own iteration
    /// order, which is an unspecified-order `HashMap` under the hood) —
    /// collecting a Y.Map's `.iter()` straight into a `BTreeMap` is already
    /// fully deterministic regardless of the source's internal order, since
    /// the target re-sorts by key on insertion.
    pub(crate) rows: BTreeMap<String, BTreeMap<String, JsonValue>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExtentValue {
    pub(crate) width: Option<f64>,
    pub(crate) height: Option<f64>,
    pub(crate) origin: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct BoardRows {
    pub(crate) schema_version: String,
    /// Read back from `resource.meta.tableOrder` — provably always equal to
    /// [`columns::TABLE_ORDER`]'s json keys (nothing ever writes a different
    /// value), so `board_export_json` uses the constant directly rather than
    /// consulting this field; it's captured here for fidelity to interfaces.md
    /// §B and so a test can assert `board_from_json` wrote it correctly.
    pub(crate) table_order: Vec<String>,
    /// Unknown top-level keys, values verbatim, in original file order — a
    /// `Vec`, deliberately not a `BTreeMap`: this is the one place row order
    /// is semantically meaningful data, not just "any deterministic order."
    pub(crate) extras: Vec<(String, JsonValue)>,
    /// keyed by DDL table name (`"workflow_links"`, not `"workflowLinks"`).
    pub(crate) tables: BTreeMap<String, TableRows>,
    /// rowId -> (x, y); `Some(None)` = register present but unparseable,
    /// `None` = register absent. Systems, workflows, trades, constraints.
    pub(crate) placement: BTreeMap<String, Option<(f64, f64)>>,
    /// rowId -> parsed extent (systems only). Same absent/unparseable
    /// distinction as `placement`.
    pub(crate) extent: BTreeMap<String, Option<ExtentValue>>,
    /// linkRowId -> the verbatim waypoints string literal (GEOM-8). The four
    /// carriers: workflowLinks, tradeLinks, constraintLinks, edges.
    pub(crate) waypoints: BTreeMap<String, String>,
    pub(crate) import_floor: Option<String>,
}

pub(crate) fn board_rows(doc: &Doc) -> BoardRows {
    let txn = doc.transact();

    let resource = txn.get_map("resource");
    let scene = txn.get_map("scene");

    let mut schema_version = "1".to_string();
    let mut table_order: Vec<String> = Vec::new();
    let mut extras: Vec<(String, JsonValue)> = Vec::new();

    if let Some(Out::YMap(meta)) = resource.as_ref().and_then(|r| r.get(&txn, "meta")) {
        if let Some(Out::Any(Any::String(s))) = meta.get(&txn, "schemaVersion") {
            schema_version = s.to_string();
        }
        if let Some(Out::Any(Any::String(s))) = meta.get(&txn, "tableOrder") {
            if let Ok(JsonValue::Array(items)) = serde_json::from_str::<JsonValue>(&s) {
                table_order = items
                    .into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
            }
        }
        if let Some(Out::Any(Any::String(s))) = meta.get(&txn, "extras") {
            if let Ok(JsonValue::Object(map)) = serde_json::from_str::<JsonValue>(&s) {
                extras = map.into_iter().collect();
            }
        }
    }

    let mut tables: BTreeMap<String, TableRows> = BTreeMap::new();
    for (_, ddl_table, _, _) in columns::TABLE_ORDER {
        let mut order: Vec<String> = Vec::new();
        let mut rows: BTreeMap<String, BTreeMap<String, JsonValue>> = BTreeMap::new();

        if let Some(Out::YMap(table_map)) = resource.as_ref().and_then(|r| r.get(&txn, ddl_table)) {
            if let Some(Out::YArray(order_arr)) = table_map.get(&txn, "order") {
                for item in order_arr.iter(&txn) {
                    if let Out::Any(Any::String(s)) = item {
                        order.push(s.to_string());
                    }
                }
            }
            if let Some(Out::YMap(rows_map)) = table_map.get(&txn, "rows") {
                rows = rows_map
                    .iter(&txn)
                    .filter_map(|(id, out)| match out {
                        Out::YMap(row_map) => {
                            let fields: BTreeMap<String, JsonValue> = row_map
                                .iter(&txn)
                                .map(|(k, v)| (k.to_string(), any_to_json_scalar_out(&v)))
                                .collect();
                            Some((id.to_string(), fields))
                        }
                        _ => None,
                    })
                    .collect();
            }
        }
        tables.insert(ddl_table.to_string(), TableRows { order, rows });
    }

    let mut placement: BTreeMap<String, Option<(f64, f64)>> = BTreeMap::new();
    let mut extent: BTreeMap<String, Option<ExtentValue>> = BTreeMap::new();
    let mut waypoints: BTreeMap<String, String> = BTreeMap::new();
    let mut import_floor: Option<String> = None;

    if let Some(scene) = &scene {
        if let Some(Out::YMap(meta)) = scene.get(&txn, "meta") {
            if let Some(Out::Any(Any::String(s))) = meta.get(&txn, "importFloor") {
                import_floor = Some(s.to_string());
            }
        }
        if let Some(Out::YMap(p)) = scene.get(&txn, "placement") {
            placement = p
                .iter(&txn)
                .map(|(id, out)| (id.to_string(), parse_placement(&out)))
                .collect();
        }
        if let Some(Out::YMap(e)) = scene.get(&txn, "extent") {
            extent = e
                .iter(&txn)
                .map(|(id, out)| (id.to_string(), parse_extent(&out)))
                .collect();
        }
        if let Some(Out::YMap(w)) = scene.get(&txn, "waypoints") {
            waypoints = w
                .iter(&txn)
                .filter_map(|(id, out)| match out {
                    Out::Any(Any::String(s)) => Some((id.to_string(), s.to_string())),
                    _ => None,
                })
                .collect();
        }
    }

    BoardRows {
        schema_version,
        table_order,
        extras,
        tables,
        placement,
        extent,
        waypoints,
        import_floor,
    }
}

fn any_to_json_scalar_out(out: &Out) -> JsonValue {
    match out {
        Out::Any(any) => any_to_json_scalar(any),
        _ => JsonValue::Null,
    }
}

fn parse_placement(out: &Out) -> Option<(f64, f64)> {
    let Out::Any(Any::String(s)) = out else {
        return None;
    };
    let v: JsonValue = serde_json::from_str(s).ok()?;
    let x = v.get("x").and_then(JsonValue::as_f64)?;
    let y = v.get("y").and_then(JsonValue::as_f64)?;
    Some((x, y))
}

fn parse_extent(out: &Out) -> Option<ExtentValue> {
    let Out::Any(Any::String(s)) = out else {
        return None;
    };
    let v: JsonValue = serde_json::from_str(s).ok()?;
    Some(ExtentValue {
        width: v.get("width").and_then(JsonValue::as_f64),
        height: v.get("height").and_then(JsonValue::as_f64),
        origin: v
            .get("origin")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

// ===========================================================================
// JS-parity JSON writer — the only place canonical-form bytes are produced.
// ===========================================================================

/// JS `Number::toString`/`JSON.stringify` number formatting. Neither Rust's
/// own `f64` `Display` nor `serde_json`'s matches it in every case: Rust's
/// `{}` already agrees with JS for ordinary values (both use a shortest
/// round-trip decimal digit sequence, and Rust already omits a trailing
/// `.0` the way JS does — `format!("{}", 1.0f64) == "1"`), but Rust's `{}`
/// (a) prints `-0` for negative zero where `JSON.stringify(-0) === "0"`, and
/// (b) never switches to exponential notation, where JS does for
/// `abs >= 1e21` or `0 < abs < 1e-6` (verified against a real `rustc` probe
/// during this unit's build, and against the five cases
/// `tests::js_number_string_matches_js_json_stringify` checks: `1`, `1.5`,
/// `0.1`, `-0`, `1e21`).
fn js_number_string(f: f64) -> String {
    if f.is_nan() || f.is_infinite() {
        // JSON.stringify(NaN) === JSON.stringify(Infinity) === "null" — not
        // expected to arise from real DDL data, but never a panic either way.
        return "null".to_string();
    }
    if f == 0.0 {
        // Covers +0.0 AND -0.0 (IEEE equality) — JSON.stringify(-0) is "0".
        return "0".to_string();
    }
    let abs = f.abs();
    if !(1e-6..1e21).contains(&abs) {
        return js_exponential(f);
    }
    format!("{f}")
}

/// `f`'s magnitude is already known to need JS exponential notation. Rust's
/// `{:e}` gives the right digits (`"<mantissa>e<exp>"`, no `+` on a positive
/// exponent); JS additionally always signs the exponent.
fn js_exponential(f: f64) -> String {
    let s = format!("{f:e}");
    match s.split_once('e') {
        Some((mantissa, exp)) => match exp.strip_prefix('-') {
            Some(digits) => format!("{mantissa}e-{digits}"),
            None => format!("{mantissa}e+{exp}"),
        },
        None => s,
    }
}

/// ECMA-262 `QuoteJSONString`: escape `"`, `\`, the mnemonic controls
/// (`\b\f\n\r\t`), every other control char as `\u00XX`; everything else —
/// including `/` and all non-ASCII — passes through raw.
fn write_json_string_value(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_opt_num(out: &mut String, v: Option<f64>) {
    match v {
        Some(n) => out.push_str(&js_number_string(n)),
        None => out.push_str("null"),
    }
}

/// Recursive: used for `extras` values, which can be any JSON shape.
fn write_json_value(out: &mut String, v: &JsonValue) {
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(n) => out.push_str(&js_number_string(n.as_f64().unwrap_or(0.0))),
        JsonValue::String(s) => write_json_string_value(out, s),
        JsonValue::Array(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_value(out, it);
            }
            out.push(']');
        }
        JsonValue::Object(map) => {
            out.push('{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string_value(out, k);
                out.push(':');
                write_json_value(out, val);
            }
            out.push('}');
        }
    }
}

// ===========================================================================
// board_export_json — canonical form C (§E) + FLOW-GARDEN-P2-amended derive
// ===========================================================================

pub(crate) struct SophiaSidecar {
    pub(crate) restore_point_id: Option<String>,
    pub(crate) graph_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct GroupExtentDelta {
    pub(crate) system_id: String,
    pub(crate) stored: (Option<f64>, Option<f64>),
    pub(crate) floor: (f64, f64),
    pub(crate) emitted: (f64, f64),
}

#[derive(Debug, Clone)]
pub(crate) struct CycleProposal {
    pub(crate) cycle: Vec<String>,
    pub(crate) suggested_detach: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DeriveReport {
    pub(crate) group_deltas: Vec<GroupExtentDelta>,
    pub(crate) cycles: Vec<CycleProposal>,
}

/// This unit's brief (item 3) types this function `-> String`; items 4 and 5
/// separately require a [`DeriveReport`] "returned beside the JSON" — an
/// internal inconsistency in the brief. [`BoardExport`] resolves it in
/// items 4/5's favor (the more specific, more detailed requirement, and the
/// one nothing else in the brief lets a caller recover otherwise) — reported
/// as this unit's deviation.
pub(crate) struct BoardExport {
    pub(crate) json: String,
    pub(crate) report: DeriveReport,
}

/// The FLOW-GARDEN-P2-amended derive report for an already-read board,
/// without producing export bytes (unit G4: the `flow.seed` result carries a
/// `DeriveReport` beside the seed stats).
pub(crate) fn board_derive_report(rows: &BoardRows) -> DeriveReport {
    derive_systems_geometry(rows).1
}

/// JSON form of a cycle-proposal list (unit G4 — shared by the two tool
/// results and by [`derive_report_json`]).
pub(crate) fn cycles_json(cycles: &[CycleProposal]) -> JsonValue {
    JsonValue::Array(
        cycles
            .iter()
            .map(|c| {
                serde_json::json!({
                    "cycle": c.cycle,
                    "suggestedDetach": c.suggested_detach,
                })
            })
            .collect(),
    )
}

/// JSON form of a [`DeriveReport`] for MCP tool results (unit G4).
pub(crate) fn derive_report_json(report: &DeriveReport) -> JsonValue {
    serde_json::json!({
        "groupDeltas": report
            .group_deltas
            .iter()
            .map(|d| {
                serde_json::json!({
                    "systemId": d.system_id,
                    "stored": { "width": d.stored.0, "height": d.stored.1 },
                    "floor": { "width": d.floor.0, "height": d.floor.1 },
                    "emitted": { "width": d.emitted.0, "height": d.emitted.1 },
                })
            })
            .collect::<Vec<_>>(),
        "cycles": cycles_json(&report.cycles),
    })
}

pub(crate) fn board_export_json(doc: &Doc, sophia: Option<SophiaSidecar>) -> BoardExport {
    let rows = board_rows(doc);
    let (systems_geom, report) = derive_systems_geometry(&rows);

    let mut out = String::new();
    out.push('{');
    out.push_str("\"schemaVersion\":");
    write_json_string_value(&mut out, &rows.schema_version);

    for (json_key, ddl_table, _, _) in columns::TABLE_ORDER {
        out.push(',');
        write_json_string_value(&mut out, json_key);
        out.push_str(":[");
        if let Some(table) = rows.tables.get(ddl_table) {
            for (i, row_id) in table.order.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_row(&mut out, ddl_table, row_id, table, &rows, &systems_geom);
            }
        }
        out.push(']');
    }

    for (k, v) in &rows.extras {
        out.push(',');
        write_json_string_value(&mut out, k);
        out.push(':');
        write_json_value(&mut out, v);
    }

    if let Some(sc) = &sophia {
        out.push_str(",\"sophia\":{\"restorePointId\":");
        match &sc.restore_point_id {
            Some(id) => write_json_string_value(&mut out, id),
            None => out.push_str("null"),
        }
        out.push_str(",\"graphId\":");
        write_json_string_value(&mut out, &sc.graph_id);
        out.push_str(",\"documentId\":");
        write_json_string_value(&mut out, BOARD_DOCUMENT_ID);
        out.push('}');
    }

    out.push('}');
    BoardExport { json: out, report }
}

fn write_row(
    out: &mut String,
    ddl_table: &str,
    row_id: &str,
    table: &TableRows,
    rows: &BoardRows,
    systems_geom: &BTreeMap<String, SystemGeom>,
) {
    out.push('{');
    let fields = table.rows.get(row_id);
    let cols = columns::ddl_columns(ddl_table);
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string_value(out, col);
        out.push(':');
        if columns::is_geometry_column(col) {
            write_geometry_value(out, ddl_table, row_id, col, rows, systems_geom);
        } else {
            match fields.and_then(|f| f.get(*col)) {
                Some(val) => write_json_value(out, val),
                // GEOM-9: never omit the key, never a DDL default — a
                // genuinely absent resource field is explicit null.
                None => out.push_str("null"),
            }
        }
    }
    out.push('}');
}

fn write_geometry_value(
    out: &mut String,
    ddl_table: &str,
    row_id: &str,
    col: &str,
    rows: &BoardRows,
    systems_geom: &BTreeMap<String, SystemGeom>,
) {
    match (ddl_table, col) {
        ("systems", "x") => write_opt_num(out, systems_geom.get(row_id).and_then(|g| g.x)),
        ("systems", "y") => write_opt_num(out, systems_geom.get(row_id).and_then(|g| g.y)),
        ("systems", "width") => write_opt_num(out, systems_geom.get(row_id).and_then(|g| g.width)),
        ("systems", "height") => {
            write_opt_num(out, systems_geom.get(row_id).and_then(|g| g.height))
        }
        (_, "x") => write_opt_num(
            out,
            rows.placement
                .get(row_id)
                .copied()
                .flatten()
                .map(|(x, _)| x),
        ),
        (_, "y") => write_opt_num(
            out,
            rows.placement
                .get(row_id)
                .copied()
                .flatten()
                .map(|(_, y)| y),
        ),
        (_, "waypoints") => match rows.waypoints.get(row_id) {
            // GEOM-8: the verbatim literal — the CONTENT is untouched
            // (never re-parsed/reformatted); it still goes through the
            // standard string writer because it is, structurally, a JSON
            // string field like any other.
            Some(w) => write_json_string_value(out, w),
            None => out.push_str("null"),
        },
        _ => out.push_str("null"),
    }
}

// ---------------------------------------------------------------------------
// FLOW-GARDEN-P2 as amended: effectiveExtent(model) = max(stored ?? default,
// fit(children)), recomputed at derive, children-before-parents.
// ---------------------------------------------------------------------------

const DEFAULT_GROUP_WIDTH: f64 = 420.0; // useModelStore.ts:18 DEFAULT_GROUP
const DEFAULT_GROUP_HEIGHT: f64 = 280.0;
const LEAF_DEFAULT_WIDTH: f64 = 200.0; // fitAncestors' `cur.width ?? 200` (:190)
const FIT_MARGIN: f64 = 20.0; // fitAncestors' `+ 20` on both axes (:192-193)
const INFO_HEIGHT_MARGIN: f64 = 12.0; // fitAncestors' `+ 12` (:191)

#[derive(Debug, Clone, Default)]
struct SystemGeom {
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
}

/// `infoHeightEstimate`, ported verbatim (useModelStore.ts:124-137). Despite
/// living inside the frontend store, it is a pure function of the row's own
/// data — never a DOM measurement — so it ports losslessly:
/// `44 + (description ? 28 : 0) + (reqs ? 14+reqs*16 : 0) + (tasks ?
/// 14+tasks*16 : 0) + (srcs ? 14+srcs*15 : 0)`.
fn info_height_estimate(
    has_description: bool,
    req_count: usize,
    task_count: usize,
    src_count: usize,
) -> f64 {
    let mut h = 44.0;
    if has_description {
        h += 28.0;
    }
    if req_count > 0 {
        h += 14.0 + (req_count as f64) * 16.0;
    }
    if task_count > 0 {
        h += 14.0 + (task_count as f64) * 16.0;
    }
    if src_count > 0 {
        h += 14.0 + (src_count as f64) * 15.0;
    }
    h
}

/// D3 cycle detection over `parent_id`, standard visited/in-progress marking
/// on the (at most singly-rooted-per-node) parent function — deterministic
/// given a deterministic input (sorted start order, and each node has
/// exactly one parent pointer, so the walk from any start is unique).
/// `parent_of` need not be systems-only-valid: a dangling `parent_id` (no
/// such row) just terminates the walk, same as a real root.
fn detect_cycles(parent_of: &BTreeMap<String, Option<String>>) -> Vec<CycleProposal> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum St {
        Unvisited,
        InProgress,
        Done,
    }
    let mut state: BTreeMap<String, St> = parent_of
        .keys()
        .cloned()
        .map(|k| (k, St::Unvisited))
        .collect();
    let mut cycles = Vec::new();
    let starts: Vec<String> = parent_of.keys().cloned().collect(); // BTreeMap: already sorted

    for start in starts {
        if state.get(&start) != Some(&St::Unvisited) {
            continue;
        }
        let mut path: Vec<String> = Vec::new();
        let mut cur: Option<String> = Some(start);
        loop {
            let Some(id) = cur else {
                for p in &path {
                    state.insert(p.clone(), St::Done);
                }
                break;
            };
            match state.get(&id).copied() {
                Some(St::Done) => {
                    for p in &path {
                        state.insert(p.clone(), St::Done);
                    }
                    break;
                }
                Some(St::InProgress) => {
                    let idx = path
                        .iter()
                        .position(|p| *p == id)
                        .expect("InProgress implies id is on the current path");
                    let mut members: Vec<String> = path[idx..].to_vec();
                    members.sort();
                    let suggested_detach = members.last().cloned().unwrap_or_default();
                    cycles.push(CycleProposal {
                        cycle: members,
                        suggested_detach,
                    });
                    for p in &path {
                        state.insert(p.clone(), St::Done);
                    }
                    break;
                }
                _ => {
                    state.insert(id.clone(), St::InProgress);
                    path.push(id.clone());
                    cur = parent_of.get(&id).cloned().flatten();
                }
            }
        }
    }
    cycles
}

/// FLOW-GARDEN-P5 / D3: "every replica detaches the lexicographically-
/// greatest node id [in a cycle] to top level" — applied here to the
/// *derive-time parent view only* (never to `resource.rows` — the model is
/// never machine-written). Clearing exactly one edge per cycle (the detached
/// member's own outgoing edge) is always sufficient to break it: a cycle of
/// any length is a closed loop of single-parent pointers, so removing one
/// link converts it into a simple acyclic chain rooted at the detached node.
fn effective_parents(
    parent_of: &BTreeMap<String, Option<String>>,
    cycles: &[CycleProposal],
) -> BTreeMap<String, Option<String>> {
    let detached: std::collections::BTreeSet<&str> =
        cycles.iter().map(|c| c.suggested_detach.as_str()).collect();
    parent_of
        .iter()
        .map(|(id, p)| {
            if detached.contains(id.as_str()) {
                (id.clone(), None)
            } else {
                (id.clone(), p.clone())
            }
        })
        .collect()
}

fn children_of(
    effective_parent: &BTreeMap<String, Option<String>>,
) -> BTreeMap<String, Vec<String>> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, parent) in effective_parent {
        if let Some(p) = parent {
            children.entry(p.clone()).or_default().push(id.clone());
        }
    }
    for v in children.values_mut() {
        v.sort();
    }
    children
}

/// The footprint a child contributes to an ancestor's floor: a group's own
/// (already-derived, memoized) `effectiveExtent`; a leaf's real stored
/// width when it has one, else the per-kind default width, paired with
/// `infoHeight+12` (the "cur.width ?? 200" / "cur.height ??
/// infoHeightEstimate+12" fallback in `fitAncestors`, generalized from a
/// single-ratchet-step read into a full children-before-parents fold — repair
/// 1, G2 must-fix #2: `stored` was ignored here entirely, so a resized
/// leaf's real width never reached its parent's floor). Only a GROUP's
/// result is exported (a leaf's own `(width, null)` is assembled directly
/// from `stored` in `derive_systems_geometry`'s final loop below, repair 1
/// must-fix #1); a leaf's return value here is used solely as a parent's
/// floor input, never itself emitted — so it never enters `deltas`.
#[allow(clippy::too_many_arguments)]
fn effective_extent(
    id: &str,
    children: &BTreeMap<String, Vec<String>>,
    pos: &BTreeMap<String, (f64, f64)>,
    stored: &BTreeMap<String, (Option<f64>, Option<f64>)>,
    info_h: &BTreeMap<String, f64>,
    memo: &mut BTreeMap<String, (f64, f64)>,
    deltas: &mut Vec<GroupExtentDelta>,
) -> (f64, f64) {
    if let Some(&v) = memo.get(id) {
        return v;
    }
    let kids = children.get(id).cloned().unwrap_or_default();
    let result = if kids.is_empty() {
        // fitAncestors' `cur.width ?? 200` (useModelStore.ts:190): a leaf's
        // OWN real stored width — human-set via NodeResizer/resizeNode
        // (SystemNode.tsx:18-21, useModelStore.ts:308-323), never derived —
        // is its true footprint contribution to a parent's floor; 200 is
        // only the fallback when no width was ever stored.
        let w = stored
            .get(id)
            .and_then(|&(w, _)| w)
            .unwrap_or(LEAF_DEFAULT_WIDTH);
        let h = info_h.get(id).copied().unwrap_or(44.0) + INFO_HEIGHT_MARGIN;
        (w, h)
    } else {
        let mut floor_w = DEFAULT_GROUP_WIDTH;
        let mut floor_h = DEFAULT_GROUP_HEIGHT;
        for child in &kids {
            let (cx, cy) = pos.get(child).copied().unwrap_or((0.0, 0.0));
            let (cw, ch) = effective_extent(child, children, pos, stored, info_h, memo, deltas);
            floor_w = floor_w.max(cx + cw + FIT_MARGIN);
            floor_h = floor_h.max(cy + ch + FIT_MARGIN);
        }
        let (stored_w, stored_h) = stored.get(id).copied().unwrap_or((None, None));
        let emitted_w = stored_w.map(|w| w.max(floor_w)).unwrap_or(floor_w);
        let emitted_h = stored_h.map(|h| h.max(floor_h)).unwrap_or(floor_h);
        deltas.push(GroupExtentDelta {
            system_id: id.to_string(),
            stored: (stored_w, stored_h),
            floor: (floor_w, floor_h),
            emitted: (emitted_w, emitted_h),
        });
        (emitted_w, emitted_h)
    };
    memo.insert(id.to_string(), result);
    result
}

fn derive_systems_geometry(rows: &BoardRows) -> (BTreeMap<String, SystemGeom>, DeriveReport) {
    let Some(systems) = rows.tables.get("systems") else {
        return (BTreeMap::new(), DeriveReport::default());
    };
    if systems.rows.is_empty() {
        return (BTreeMap::new(), DeriveReport::default());
    }

    let mut parent_of: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut pos: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    let mut stored: BTreeMap<String, (Option<f64>, Option<f64>)> = BTreeMap::new();
    let mut has_desc: BTreeMap<String, bool> = BTreeMap::new();

    for (id, fields) in &systems.rows {
        let parent_id = fields
            .get("parent_id")
            .and_then(JsonValue::as_str)
            .map(|s| s.to_string());
        parent_of.insert(id.clone(), parent_id);

        let desc_nonempty = fields
            .get("description")
            .and_then(JsonValue::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        has_desc.insert(id.clone(), desc_nonempty);

        let (x, y) = rows
            .placement
            .get(id)
            .copied()
            .flatten()
            .unwrap_or((0.0, 0.0));
        pos.insert(id.clone(), (x, y));

        let (w, h) = rows
            .extent
            .get(id)
            .and_then(|e| e.as_ref())
            .map(|e| (e.width, e.height))
            .unwrap_or((None, None));
        stored.insert(id.clone(), (w, h));
    }

    let mut req_counts: BTreeMap<String, usize> = BTreeMap::new();
    if let Some(t) = rows.tables.get("requirements") {
        for f in t.rows.values() {
            if let Some(sid) = f.get("system_id").and_then(JsonValue::as_str) {
                *req_counts.entry(sid.to_string()).or_insert(0) += 1;
            }
        }
    }
    let mut task_counts: BTreeMap<String, usize> = BTreeMap::new();
    if let Some(t) = rows.tables.get("tasks") {
        for f in t.rows.values() {
            let is_system_owned = f.get("owner_type").and_then(JsonValue::as_str) == Some("system");
            if is_system_owned {
                if let Some(oid) = f.get("owner_id").and_then(JsonValue::as_str) {
                    *task_counts.entry(oid.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    let mut src_counts: BTreeMap<String, usize> = BTreeMap::new();
    if let Some(t) = rows.tables.get("sources") {
        for f in t.rows.values() {
            if let Some(oid) = f.get("owner_id").and_then(JsonValue::as_str) {
                *src_counts.entry(oid.to_string()).or_insert(0) += 1;
            }
        }
    }

    let info_h: BTreeMap<String, f64> = parent_of
        .keys()
        .map(|id| {
            let h = info_height_estimate(
                has_desc.get(id).copied().unwrap_or(false),
                req_counts.get(id).copied().unwrap_or(0),
                task_counts.get(id).copied().unwrap_or(0),
                src_counts.get(id).copied().unwrap_or(0),
            );
            (id.clone(), h)
        })
        .collect();

    let cycles = detect_cycles(&parent_of);
    let eff_parent = effective_parents(&parent_of, &cycles);
    let children = children_of(&eff_parent);

    let mut memo: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    let mut deltas: Vec<GroupExtentDelta> = Vec::new();
    for id in parent_of.keys() {
        effective_extent(
            id,
            &children,
            &pos,
            &stored,
            &info_h,
            &mut memo,
            &mut deltas,
        );
    }
    deltas.sort_by(|a, b| a.system_id.cmp(&b.system_id));

    let mut out: BTreeMap<String, SystemGeom> = BTreeMap::new();
    for id in parent_of.keys() {
        let xy = rows.placement.get(id).copied().flatten();
        let is_group = children.get(id).map(|c| !c.is_empty()).unwrap_or(false);
        let (width, height) = if is_group {
            let (ew, eh) = memo
                .get(id)
                .copied()
                .unwrap_or((DEFAULT_GROUP_WIDTH, DEFAULT_GROUP_HEIGHT));
            (Some(ew), Some(eh))
        } else {
            // EF-7 ("Leaves carry no stored height") names height only —
            // width IS real, human-settable data for a leaf: NodeResizer
            // renders unconditionally on every system node (SystemNode.tsx
            // :18-21, only minWidth/minHeight differ by hasChildren);
            // resizeNode sets a leaf's real `width` from the drag while
            // forcing only `height` to null (useModelStore.ts:308-323,
            // "leaf height stays content-driven; only groups keep a manual
            // height"); toFlow.ts:104-106 reads `s.width` for a leaf when
            // it is non-null. Pass the leaf's own stored width through
            // unchanged (repair 1, G2 must-fix #1) rather than discarding
            // it — height stays forced to `None` per EF-7.
            let w = stored.get(id).and_then(|&(w, _)| w);
            (w, None)
        };
        out.insert(
            id.clone(),
            SystemGeom {
                x: xy.map(|(x, _)| x),
                y: xy.map(|(_, y)| y),
                width,
                height,
            },
        );
    }

    (
        out,
        DeriveReport {
            group_deltas: deltas,
            cycles,
        },
    )
}

// ===========================================================================
// board_desired_triples — resource → flow: projection (interfaces.md §G)
// ===========================================================================

/// The `resource → flow:` triples for the whole board, per
/// `contracts/vocabulary-map.md`: one `rdf:type` triple per row plus one
/// triple per materialized, non-null predicate ([`vocab::predicates`]).
/// Geometry is never a predicate here (interfaces.md §C); `display_mode` is
/// (`flow:displayMode`, on `flow:System`).
///
/// A `uri`-datatype predicate's object is a *class-qualified* subject IRI
/// (`vocabulary-map.md`'s `subject_rule`), resolved by looking the
/// referenced id up across the WHOLE board (Flow mints every id from one
/// global `uuid()`, never reused across tables, so this lookup is
/// unambiguous) — several of these predicates are declared `sh:or` unions
/// (`ownedBy`, `target`, `sourceNode`/`targetNode`, `predecessor`/
/// `successor`), so no single fixed target class would be correct. A
/// reference to an id that isn't any row on this board (a dangling
/// reference) is skipped rather than guessed at or fabricated.
///
/// Sorted by `(subject, predicate, object N-Triples form)` — `Term` has no
/// `Ord` of its own, so the object's N-Triples string stands in as the
/// third sort key.
pub(crate) fn board_desired_triples(rows: &BoardRows, graph_subject: &str) -> Vec<Triple> {
    let mut id_table: BTreeMap<&str, &str> = BTreeMap::new();
    for (ddl_table, table) in &rows.tables {
        for id in table.rows.keys() {
            // should-fix (repair 1): self-check the one-global-uuid()
            // assumption this table rests on — Flow never reuses an id
            // across tables, so this insert must never overwrite an entry.
            // Debug-only: a release build still degrades to the documented
            // (alphabetically-last-wins) behavior rather than panicking.
            let prev_table = id_table.insert(id.as_str(), ddl_table.as_str());
            debug_assert!(
                prev_table.is_none(),
                "Flow row id {:?} reused across tables ({:?} then {:?}) — \
                 id_table's one-global-uuid() assumption is violated",
                id,
                prev_table,
                ddl_table
            );
        }
    }

    let mut triples: Vec<Triple> = Vec::new();
    for (ddl_table, table) in &rows.tables {
        let Some(kebab) = columns::kebab_class(ddl_table) else {
            continue;
        };
        let Some(pascal) = columns::pascal_class(ddl_table) else {
            continue;
        };
        for (row_id, fields) in &table.rows {
            let subject = subject_iri(graph_subject, kebab, row_id);
            let class_term = term_for(&Value::Uri(format!("{FLOW_NS}{pascal}")), Datatype::uri);
            triples.push((subject.clone(), RDF_TYPE_URI.to_string(), class_term));

            for p in vocab::predicates(ddl_table) {
                let Some(raw) = fields.get(p.column) else {
                    continue;
                };
                if raw.is_null() {
                    continue;
                }
                let Some(term) = triple_object(raw, p.datatype, graph_subject, &id_table) else {
                    continue;
                };
                let predicate_iri = format!("{FLOW_NS}{}", p.local);
                triples.push((subject.clone(), predicate_iri, term));
            }
        }
    }

    triples.sort_by(|a, b| {
        (a.0.as_str(), a.1.as_str(), a.2.as_nt()).cmp(&(b.0.as_str(), b.1.as_str(), b.2.as_nt()))
    });
    triples
}

fn subject_iri(graph_subject: &str, kebab_class: &str, local_id: &str) -> String {
    format!("{graph_subject}:projection:flow:{kebab_class}:{local_id}")
}

fn triple_object(
    raw: &JsonValue,
    datatype: Datatype,
    graph_subject: &str,
    id_table: &BTreeMap<&str, &str>,
) -> Option<Term> {
    match datatype {
        Datatype::uri => {
            let target_id = raw.as_str()?;
            let target_table = *id_table.get(target_id)?; // dangling reference: no triple
            let target_kebab = columns::kebab_class(target_table)?;
            let object_iri = subject_iri(graph_subject, target_kebab, target_id);
            Some(term_for(&Value::Uri(object_iri), Datatype::uri))
        }
        Datatype::string => {
            let s = raw.as_str().map(|s| s.to_string()).unwrap_or_default();
            Some(term_for(&Value::Str(s), Datatype::string))
        }
        Datatype::integer => {
            let n = raw.as_f64().unwrap_or(0.0) as i64;
            Some(term_for(&Value::Int(n), Datatype::integer))
        }
        Datatype::boolean => {
            let b = raw.as_bool().unwrap_or(false);
            Some(term_for(&Value::Bool(b), Datatype::boolean))
        }
        // long/float/double/dateTime never appear in this pack (RTRIP-6;
        // interfaces.md §C excludes the only DOUBLE columns as geometry).
        _ => None,
    }
}

// ===========================================================================
// Decode helpers — a stored update-v1 payload → a scratch board Doc (unit G3)
// ===========================================================================

/// Decode one encoded-as-update-v1 Y.Doc state (the exact bytes the room
/// persists as `update-v1.bin` / inlines as `ydocUpdateBase64`) into a fresh
/// scratch [`Doc`], ready for [`board_rows`]/[`board_export_json`]/
/// [`board_desired_triples`]. Empty input is a valid empty board (a fresh
/// `Doc` with no roots — every table reads back empty).
pub(crate) fn board_doc_from_update_bytes(bytes: &[u8]) -> Result<Doc, String> {
    use yrs::updates::decoder::Decode as _;
    let doc = Doc::new();
    if bytes.is_empty() {
        return Ok(doc);
    }
    let update = yrs::Update::decode_v1(bytes)
        .map_err(|error| format!("decode flow board Y.Doc update: {error}"))?;
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(update)
            .map_err(|error| format!("apply flow board Y.Doc update: {error}"))?;
    }
    Ok(doc)
}

/// [`board_doc_from_update_bytes`] over the base64 inline form
/// (`DocumentRecord.ydoc_update_base64`). An empty string is a valid empty
/// board, mirroring the byte form.
pub(crate) fn board_doc_from_update_base64(encoded: &str) -> Result<Doc, String> {
    if encoded.is_empty() {
        return Ok(Doc::new());
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| format!("decode flow board Y.Doc base64: {error}"))?;
    board_doc_from_update_bytes(&bytes)
}
