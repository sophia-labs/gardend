//! Block-level operations (block.insert/update/editText/delete) for
//! headless cells. Port of the applyBlock* handlers in
//! frontend/src/native/native-local-runtime.ts plus
//! frontend/src/crdt/block-mutations.ts and operation-ledger.ts.
//!
//! Mutations flow through the room registry (live collaborators see them as
//! normal sync updates), then the full document is materialized and persisted
//! via the same save_document path the desktop frontend uses. The TS runtime
//! defers materialization to the editor channel's debounced save; headless
//! cells have no channel, so we materialize eagerly after each mutation.
//!
//! Replay safety: block.insert and block.editText are guarded by the
//! per-document operation ledger stored *inside* the Y.Doc (Y.Map
//! `_operationLog`), co-written transactionally with the mutation — a port of
//! frontend/src/crdt/operation-ledger.ts including its FNV-1a-over-UTF-16
//! canonical payload hash.
//!
//! Offset semantics: payload text offsets are JS string offsets (UTF-16 code
//! units), exactly as the TS handlers interpret them. The room Doc uses yrs'
//! default byte offsets, so every Y.Text call converts UTF-16 → UTF-8 byte
//! offsets against the segment's current content.

use crate::app_runtime::AppHandle;
use crate::crdt_queue::CrdtOperation;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::types::text::YChange;
use yrs::types::xml::{XmlFragment, XmlOut};
use yrs::types::Attrs;
use yrs::{
    Any, In, Map as YMap, MapPrelim, MapRef, Out, ReadTxn, RootRef, Text, TransactionMut, Xml,
    XmlElementPrelim, XmlElementRef, XmlFragmentRef, XmlTextPrelim, XmlTextRef,
};

use super::executor::{ApplyOperationError, ApplyOperationResult};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub(crate) async fn apply(app: &AppHandle, operation: &CrdtOperation) -> Result<Value, String> {
    apply_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    match operation.kind.as_str() {
        "block.insert" => block_insert(app, operation).await,
        "block.update" => block_update(app, operation).await,
        "block.editText" => block_edit_text(app, operation).await,
        "block.delete" => block_delete(app, operation).await,
        other => Err(ApplyOperationError::terminal(format!(
            "unsupported block operation: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// JS payload coercion helpers (ports of objectPayload/stringValue/
// finiteNumber/stringArrayValue and the `a ?? b` access chains)
// ---------------------------------------------------------------------------

fn obj(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// JS `map.a ?? map.b ?? …`: first key whose value is present and non-null.
fn coalesce<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        match map.get(*key) {
            None | Some(Value::Null) => continue,
            Some(value) => return Some(value),
        }
    }
    None
}

/// Port of stringValue/nullableStringValue: undefined/null/'' → None,
/// otherwise String(value).
pub(crate) fn js_string_value(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(value) => Some(js_string(value)),
    }
}

/// JS String(value) coercion for JSON values.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => js_f64_string(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn js_f64_string(n: f64) -> String {
    if n == 0.0 {
        "0".to_string()
    } else if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// JS truthiness.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Port of finiteNumber: undefined/null/'' → None; Number(value) when finite.
pub(crate) fn js_finite_number(value: Option<&Value>) -> Option<f64> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Port of stringArrayValue.
fn js_string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| js_string_value(Some(item)))
                .collect()
        })
        .unwrap_or_default()
}

/// JSON → yrs Any (same conversion as builder::json_to_any; duplicated so the
/// pure core of this module stays self-contained for standalone tests).
fn json_to_any(value: &Value) -> Any {
    match value {
        Value::Null => Any::Null,
        Value::Bool(b) => Any::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Any::Number(i as f64)
            } else {
                Any::Number(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => Any::from(s.as_str()),
        Value::Array(items) => Any::Array(items.iter().map(json_to_any).collect()),
        Value::Object(entries) => Any::Map(Arc::new(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), json_to_any(v)))
                .collect(),
        )),
    }
}

fn any_to_json(any: &Any) -> Value {
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => json!(b),
        Any::Number(n) => json!(n),
        Any::BigInt(n) => json!(n),
        Any::String(s) => json!(s.as_ref()),
        Any::Buffer(b) => json!(b.as_ref()),
        Any::Array(items) => Value::Array(items.iter().map(any_to_json).collect()),
        Any::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), any_to_json(v)))
                .collect(),
        ),
    }
}

fn out_to_json(out: &Out) -> Value {
    match out {
        Out::Any(any) => any_to_json(any),
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Canonical payload hash (port of operation-ledger.ts canonicalPayloadHash:
// sorted-key canonical JSON, FNV-1a 32-bit over UTF-16 code units)
// ---------------------------------------------------------------------------

pub fn canonical_payload_hash(payload: &Value) -> String {
    format!("{:08x}", fnv1a32_utf16(&canonicalize(payload)))
}

fn canonicalize(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Number(n) => js_json_number(n),
        Value::String(s) => json_stringify_string(s),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(canonicalize).collect::<Vec<_>>().join(",")
        ),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        json_stringify_string(key),
                        canonicalize(&map[*key])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// JSON.stringify for a string (serde_json escaping matches JSON.stringify
/// for all JSON-representable strings).
fn json_stringify_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// JSON.stringify for a number. Integers print without a fraction; floats use
/// shortest round-trip formatting (Rust's Display, which matches V8 for the
/// non-exponential range payloads actually use).
fn js_json_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else {
        js_f64_string(n.as_f64().unwrap_or(0.0))
    }
}

/// FNV-1a 32-bit over UTF-16 code units — replicates the TS implementation
/// (`s.charCodeAt(i)` + `Math.imul(h, 0x01000193) >>> 0`).
fn fnv1a32_utf16(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for unit in s.encode_utf16() {
        h ^= unit as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---------------------------------------------------------------------------
// Operation ledger (port of operation-ledger.ts; stored inside the Y.Doc)
// ---------------------------------------------------------------------------

pub(crate) const LEDGER_MAP_NAME: &str = "_operationLog";
const LEDGER_SCHEMA_VERSION: f64 = 1.0;
pub(crate) const LEDGER_MAX_ENTRIES_PER_DOC: usize = 1024;
const LEDGER_MAX_AGE_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
const LEDGER_RESULT_MAX_BYTES: usize = 4096;

pub struct LedgerHit {
    pub result: Option<Value>,
    pub payload_hash: Option<String>,
    #[allow(dead_code)]
    pub completed_at: String,
}

pub struct LedgerWriteFields<'a> {
    pub op: &'a str,
    pub completed_at: &'a str,
    pub payload_hash: Option<String>,
    pub result: Option<&'a Value>,
}

fn ledger_string(map: &MapRef, txn: &TransactionMut<'_>, key: &str) -> Option<String> {
    match map.get(txn, key) {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

/// Port of readLedgerEntry. None ⇒ the operationId has not been applied.
pub fn read_ledger_entry(txn: &TransactionMut<'_>, operation_id: &str) -> Option<LedgerHit> {
    let ledger = txn.get_map(LEDGER_MAP_NAME)?;
    let entry = match ledger.get(txn, operation_id) {
        Some(Out::YMap(entry)) => entry,
        _ => return None,
    };
    let version_ok = match entry.get(txn, "v") {
        Some(Out::Any(Any::Number(n))) => n == LEDGER_SCHEMA_VERSION,
        Some(Out::Any(Any::BigInt(n))) => n as f64 == LEDGER_SCHEMA_VERSION,
        _ => false,
    };
    if !version_ok {
        // Permissive parsing: unknown schema version still counts as
        // "applied", but carries no usable result or hash.
        return Some(LedgerHit {
            result: None,
            payload_hash: None,
            completed_at: String::new(),
        });
    }
    let result = ledger_string(&entry, txn, "result")
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    Some(LedgerHit {
        result,
        payload_hash: ledger_string(&entry, txn, "payloadHash"),
        completed_at: ledger_string(&entry, txn, "completedAt").unwrap_or_default(),
    })
}

/// Port of writeLedgerEntry. Must run in the same transaction as the
/// mutation it guards.
pub fn write_ledger_entry(
    txn: &mut TransactionMut<'_>,
    operation_id: &str,
    fields: LedgerWriteFields<'_>,
) {
    let ledger = MapRef::root(LEDGER_MAP_NAME).get_or_create(txn);
    let mut entry: Vec<(Arc<str>, In)> = vec![
        ("v".into(), In::Any(Any::Number(LEDGER_SCHEMA_VERSION))),
        ("op".into(), In::Any(Any::from(fields.op))),
        (
            "completedAt".into(),
            In::Any(Any::from(fields.completed_at)),
        ),
    ];
    if let Some(hash) = fields.payload_hash {
        entry.push(("payloadHash".into(), In::Any(Any::from(hash))));
    }
    if let Some(result) = fields.result {
        if let Ok(serialized) = serde_json::to_string(result) {
            // TS drops results larger than RESULT_MAX_BYTES (s.length is
            // UTF-16 units) silently.
            if utf16_len(&serialized) <= LEDGER_RESULT_MAX_BYTES {
                entry.push(("result".into(), In::Any(Any::from(serialized))));
            }
        }
    }
    ledger.insert(txn, operation_id, MapPrelim::from_iter(entry));
}

/// Port of pruneLedger (default options). Returns entries removed.
///
/// Deviation from TS: completedAt parses as epoch-millis first (the format
/// this queue actually stamps), then as ISO-8601; TS used Date.parse, which
/// yields NaN for epoch-millis strings.
pub fn prune_ledger(txn: &mut TransactionMut<'_>, now_ms: f64) -> usize {
    let ledger = MapRef::root(LEDGER_MAP_NAME).get_or_create(txn);
    let cutoff = now_ms - LEDGER_MAX_AGE_MS;

    let mut entries: Vec<(String, f64)> = Vec::new();
    for (key, value) in ledger.iter(txn) {
        let mut completed_at_ms = 0.0_f64;
        if let Out::YMap(map) = value {
            if let Some(Out::Any(Any::String(raw))) = map.get(txn, "completedAt") {
                if let Some(parsed) = parse_completed_at_ms(&raw) {
                    completed_at_ms = parsed;
                }
            }
        }
        entries.push((key.to_string(), completed_at_ms));
    }

    let mut to_drop: HashSet<String> = entries
        .iter()
        .filter(|(_, ms)| *ms < cutoff)
        .map(|(key, _)| key.clone())
        .collect();

    let mut surviving: Vec<&(String, f64)> = entries
        .iter()
        .filter(|(key, _)| !to_drop.contains(key))
        .collect();
    if surviving.len() > LEDGER_MAX_ENTRIES_PER_DOC {
        surviving.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let overflow = surviving.len() - LEDGER_MAX_ENTRIES_PER_DOC;
        for item in surviving.iter().take(overflow) {
            to_drop.insert(item.0.clone());
        }
    }

    for key in &to_drop {
        ledger.remove(txn, key);
    }
    to_drop.len()
}

fn parse_completed_at_ms(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(ms) = trimmed.parse::<f64>() {
        if ms.is_finite() {
            return Some(ms);
        }
    }
    parse_iso8601_ms(trimmed)
}

/// Minimal ISO-8601 → epoch-millis parser:
/// `YYYY-MM-DD(T| )HH:MM(:SS(.fff)?)?(Z|±hh(:)?mm)?`.
fn parse_iso8601_ms(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut hour, mut minute, mut second, mut millis, mut offset_min) =
        (0i64, 0i64, 0i64, 0i64, 0i64);
    if bytes.len() > 10 {
        if bytes[10] != b'T' && bytes[10] != b' ' {
            return None;
        }
        let mut rest = &s[11..];
        // Split off the timezone suffix, if any.
        if let Some(stripped) = rest.strip_suffix('Z') {
            rest = stripped;
        } else if let Some(pos) = rest.rfind(['+', '-']) {
            // Only treat as a zone offset when it appears after the time.
            if pos >= 5 {
                let zone = &rest[pos..];
                let sign = if zone.starts_with('-') { -1 } else { 1 };
                let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
                if digits.len() == 4 {
                    let zh: i64 = digits[0..2].parse().ok()?;
                    let zm: i64 = digits[2..4].parse().ok()?;
                    offset_min = sign * (zh * 60 + zm);
                    rest = &rest[..pos];
                }
            }
        }
        let mut time_parts = rest.splitn(3, ':');
        hour = time_parts.next()?.parse().ok()?;
        minute = time_parts.next()?.parse().ok()?;
        if let Some(sec_part) = time_parts.next() {
            let mut sec_split = sec_part.splitn(2, '.');
            second = sec_split.next()?.parse().ok()?;
            if let Some(frac) = sec_split.next() {
                let frac: String = frac.chars().take(3).collect();
                let scale = 10_i64.pow(3 - frac.len() as u32);
                millis = frac.parse::<i64>().ok()? * scale;
            }
        }
    }
    let days = days_from_civil(year, month, day);
    Some(
        (((days * 86_400 + hour * 3_600 + minute * 60 + second - offset_min * 60) * 1_000) + millis)
            as f64,
    )
}

/// Howard Hinnant's days-from-civil algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn epoch_ms_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Fragment / block lookup helpers (port of block-mutations.ts)
// ---------------------------------------------------------------------------

fn content_fragment(txn: &mut TransactionMut<'_>) -> XmlFragmentRef {
    XmlFragmentRef::root("content").get_or_create(txn)
}

/// Port of readBlockId: data-block-id ?? blockId, non-empty string only.
fn read_block_id_from_element<T: ReadTxn>(txn: &T, element: &XmlElementRef) -> Option<String> {
    let primary = element.get_attribute(txn, "data-block-id");
    let raw = match &primary {
        None | Some(Out::Any(Any::Null)) | Some(Out::Any(Any::Undefined)) => {
            element.get_attribute(txn, "blockId")
        }
        Some(_) => primary.clone(),
    };
    match raw {
        Some(Out::Any(Any::String(s))) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

/// Port of readBlockIdFromJson.
pub fn read_block_id_from_json(node: &Value) -> Option<String> {
    let attrs = node.get("attrs").and_then(Value::as_object)?;
    let id = match attrs.get("data-block-id") {
        None | Some(Value::Null) => attrs.get("blockId"),
        id => id,
    };
    match id {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Port of findBlockInFragment.
pub fn find_block_in_fragment<T: ReadTxn>(
    txn: &T,
    fragment: &XmlFragmentRef,
    block_id: &str,
) -> Option<(XmlElementRef, u32)> {
    for (index, child) in fragment.children(txn).enumerate() {
        if let XmlOut::Element(element) = child {
            if read_block_id_from_element(txn, &element).as_deref() == Some(block_id) {
                return Some((element, index as u32));
            }
        }
    }
    None
}

/// Port of indexBlocksInFragment (first occurrence wins).
fn index_blocks_in_fragment<T: ReadTxn>(
    txn: &T,
    fragment: &XmlFragmentRef,
) -> HashMap<String, (XmlElementRef, u32)> {
    let mut index_by_id = HashMap::new();
    for (index, child) in fragment.children(txn).enumerate() {
        if let XmlOut::Element(element) = child {
            if let Some(id) = read_block_id_from_element(txn, &element) {
                index_by_id.entry(id).or_insert((element, index as u32));
            }
        }
    }
    index_by_id
}

/// Port of blockIdsInFragment.
pub fn block_ids_in_fragment<T: ReadTxn>(txn: &T, fragment: &XmlFragmentRef) -> Vec<String> {
    let mut ids = Vec::new();
    for child in fragment.children(txn) {
        if let XmlOut::Element(element) = child {
            if let Some(id) = read_block_id_from_element(txn, &element) {
                ids.push(id);
            }
        }
    }
    ids
}

// ---------------------------------------------------------------------------
// Insert machinery (port of buildBlockElement/insertBlocksIntoFragment and
// the applyBlockInsert position resolution)
// ---------------------------------------------------------------------------

/// Port of clampIndex: Math.max(0, Math.min(Math.floor(value), length)),
/// non-finite → length.
pub fn clamp_index(value: f64, length: u32) -> u32 {
    if !value.is_finite() {
        return length;
    }
    let floored = value.floor();
    if floored <= 0.0 {
        0
    } else if floored >= length as f64 {
        length
    } else {
        floored as u32
    }
}

/// Port of applyBlockInsert's insertAt resolution. `located` is the index of
/// the reference block when refBlockId resolved (the caller errors when a
/// requested refBlockId is missing).
pub fn resolve_insert_index(
    located: Option<u32>,
    position_before: bool,
    index_hint: Option<f64>,
    block_count: u32,
) -> f64 {
    if let Some(index) = located {
        return if position_before {
            index as f64
        } else {
            index as f64 + 1.0
        };
    }
    if let Some(hint) = index_hint {
        return if hint < 0.0 {
            (block_count as f64 + 1.0 + hint).max(0.0)
        } else {
            hint.min(block_count as f64)
        };
    }
    block_count as f64
}

/// Port of marksToTextAttributes (every TIPTAP_MARK_TO_ATTR entry maps a key
/// to itself, so the lookup collapses to the mark type).
fn marks_to_text_attributes(marks: Option<&Value>) -> Attrs {
    let mut result = Attrs::new();
    if let Some(list) = marks.and_then(Value::as_array) {
        for mark in list {
            let Some(mark_type) = mark.get("type").and_then(Value::as_str) else {
                continue;
            };
            let attrs_any = match mark.get("attrs") {
                Some(attrs) if js_truthy(attrs) => json_to_any(attrs),
                _ => Any::Map(Arc::new(Default::default())),
            };
            result.insert(Arc::from(mark_type), attrs_any);
        }
    }
    result
}

/// Port of buildBlockElement, integrated directly at `index`. Text nodes
/// always materialize a Y.XmlText child (even when empty), matching the TS
/// builder; attributes skip null values.
fn insert_tiptap_node_at<F: XmlFragment>(
    txn: &mut TransactionMut<'_>,
    parent: &F,
    index: u32,
    node: &Value,
) {
    if node.get("type").and_then(Value::as_str) == Some("text") {
        let text_ref = parent.insert(txn, index, XmlTextPrelim::new(""));
        let text = node.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.is_empty() {
            let attrs = marks_to_text_attributes(node.get("marks"));
            if attrs.is_empty() {
                text_ref.insert(txn, 0, text);
            } else {
                text_ref.insert_with_attributes(txn, 0, text, attrs);
            }
        }
        return;
    }

    let tag = node
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("paragraph");
    let element = parent.insert(txn, index, XmlElementPrelim::empty(tag));
    if let Some(attrs) = node.get("attrs").and_then(Value::as_object) {
        for (key, value) in attrs {
            if value.is_null() {
                continue;
            }
            element.insert_attribute(txn, key.clone(), json_to_any(value));
        }
    }
    if let Some(content) = node.get("content").and_then(Value::as_array) {
        for (child_index, child) in content.iter().enumerate() {
            insert_tiptap_node_at(txn, &element, child_index as u32, child);
        }
    }
}

fn enrich_with_block_id(block: &Value, block_id: &str) -> Value {
    let mut node = obj(block);
    let mut attrs = node
        .get("attrs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    attrs.insert("data-block-id".to_string(), json!(block_id));
    node.insert("attrs".to_string(), Value::Object(attrs));
    Value::Object(node)
}

/// Port of insertBlocksIntoFragment. Validates every block (id present,
/// element at top level) before mutating, so error paths leave the fragment
/// untouched.
pub fn insert_blocks_into_fragment(
    txn: &mut TransactionMut<'_>,
    fragment: &XmlFragmentRef,
    blocks: &[Value],
    position: Option<f64>,
) -> Result<Vec<String>, String> {
    let length = fragment.len(txn);
    let index = clamp_index(position.unwrap_or(length as f64), length);
    let mut ids = Vec::with_capacity(blocks.len());
    for block in blocks {
        let block_id = read_block_id_from_json(block).ok_or_else(|| {
            "block.insert: every block must carry a data-block-id; supply IDs at the enqueue path"
                .to_string()
        })?;
        if block.get("type").and_then(Value::as_str) == Some("text") {
            return Err(
                "block.insert: top-level entries must be block elements, got text node".to_string(),
            );
        }
        ids.push(block_id);
    }
    for (offset, block) in blocks.iter().enumerate() {
        let enriched = enrich_with_block_id(block, &ids[offset]);
        insert_tiptap_node_at(txn, fragment, index + offset as u32, &enriched);
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Text edit machinery (port of editBlockText + helpers)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum BlockTextEditOp {
    Insert {
        offset: f64,
        text: String,
        /// Some ⇒ the `attrs` key was present in the payload (even if null,
        /// mirroring `data.attrs === undefined ? undefined : objectPayload(…)`).
        attrs: Option<Map<String, Value>>,
        inherit_format: Option<bool>,
    },
    Delete {
        offset: f64,
        length: f64,
    },
}

impl BlockTextEditOp {
    fn offset(&self) -> f64 {
        match self {
            BlockTextEditOp::Insert { offset, .. } | BlockTextEditOp::Delete { offset, .. } => {
                *offset
            }
        }
    }
}

#[derive(Debug)]
pub struct BlockTextEditResult {
    pub block_id: String,
    pub length_before: usize,
    pub length_after: usize,
    pub applied: usize,
}

pub(crate) struct TextSegment {
    pub(crate) text: XmlTextRef,
    /// Start offset in UTF-16 units, frozen at collection time (TS parity).
    pub(crate) start: usize,
    /// Length in UTF-16 units, frozen at collection time.
    pub(crate) length: usize,
}

pub(crate) fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// UTF-16 offset → UTF-8 byte offset against `s`, clamped to char
/// boundaries/string end (offsets landing inside a surrogate pair round up).
pub(crate) fn utf16_to_byte_offset(s: &str, utf16_offset: usize) -> usize {
    if utf16_offset == 0 {
        return 0;
    }
    let mut units = 0usize;
    for (byte_index, ch) in s.char_indices() {
        if units >= utf16_offset {
            return byte_index;
        }
        units += ch.len_utf16();
    }
    s.len()
}

/// Plain-text content of an XmlText (delta chunks joined).
pub(crate) fn xml_text_content<T: ReadTxn>(txn: &T, text: &XmlTextRef) -> String {
    let mut out = String::new();
    for diff in text.diff(txn, YChange::identity) {
        if let Out::Any(Any::String(chunk)) = diff.insert {
            out.push_str(&chunk);
        }
    }
    out
}

/// Port of collectTextSegments: depth-first walk gathering Y.XmlText leaves
/// in document order with cumulative UTF-16 offsets.
pub(crate) fn collect_text_segments<T: ReadTxn>(
    txn: &T,
    element: &XmlElementRef,
) -> Vec<TextSegment> {
    let mut segments = Vec::new();
    let mut stack: Vec<XmlOut> = element.children(txn).collect();
    stack.reverse();
    let mut cursor = 0usize;
    while let Some(node) = stack.pop() {
        match node {
            XmlOut::Text(text) => {
                let length = utf16_len(&xml_text_content(txn, &text));
                segments.push(TextSegment {
                    text,
                    start: cursor,
                    length,
                });
                cursor += length;
            }
            XmlOut::Element(child) => {
                let mut children: Vec<XmlOut> = child.children(txn).collect();
                children.reverse();
                stack.extend(children);
            }
            XmlOut::Fragment(_) => {}
        }
    }
    segments
}

/// Port of locateTextSegment (preferLeft variant is the only one used here).
fn locate_text_segment<'a>(
    segments: &'a [TextSegment],
    global_offset: f64,
    total_length: usize,
    prefer_left: bool,
) -> Result<(&'a TextSegment, f64), String> {
    if segments.is_empty() {
        return Err("block.editText: block has no text segments to edit".to_string());
    }
    let clamped = global_offset.max(0.0).min(total_length as f64);
    for segment in segments {
        let start = segment.start as f64;
        let end = (segment.start + segment.length) as f64;
        let within = if prefer_left {
            clamped > start && clamped <= end
        } else {
            clamped >= start && clamped < end
        };
        if within {
            return Ok((segment, clamped - start));
        }
    }
    if clamped == 0.0 {
        return Ok((&segments[0], 0.0));
    }
    let last = segments.last().expect("non-empty segments");
    Ok((last, last.length as f64))
}

/// Port of toFormatAttrs: skip null values; None when nothing remains.
pub(crate) fn to_format_attrs(attrs: &Map<String, Value>) -> Option<Attrs> {
    let mut result = Attrs::new();
    for (key, value) in attrs {
        if value.is_null() {
            continue;
        }
        result.insert(Arc::from(key.as_str()), json_to_any(value));
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Port of formatAttrsAt: formatting attributes of the delta chunk covering
/// `utf16_offset` (used to inherit format when inserting mid-text).
fn format_attrs_at<T: ReadTxn>(txn: &T, text: &XmlTextRef, utf16_offset: f64) -> Option<Attrs> {
    if utf16_offset <= 0.0 {
        return None;
    }
    let mut cursor = 0usize;
    for diff in text.diff(txn, YChange::identity) {
        let Out::Any(Any::String(chunk)) = diff.insert else {
            continue;
        };
        let next = cursor + utf16_len(&chunk);
        if utf16_offset <= next as f64 {
            return diff.attributes.map(|boxed| (*boxed).clone());
        }
        cursor = next;
    }
    None
}

/// Port of editBlockText. Operations apply in descending-offset order
/// (ties: later payload entries first), against segment offsets frozen at
/// collection time — exactly the TS semantics.
pub fn edit_block_text(
    txn: &mut TransactionMut<'_>,
    element: &XmlElementRef,
    operations: &[BlockTextEditOp],
) -> Result<BlockTextEditResult, String> {
    if operations.is_empty() {
        return Err("block.editText: operations must not be empty".to_string());
    }

    let segments = collect_text_segments(txn, element);
    let block_id = read_block_id_from_element(txn, element).unwrap_or_default();
    let length_before: usize = segments.iter().map(|s| s.length).sum();

    let mut order: Vec<usize> = (0..operations.len()).collect();
    order.sort_by(|&a, &b| {
        let (offset_a, offset_b) = (operations[a].offset(), operations[b].offset());
        match offset_b
            .partial_cmp(&offset_a)
            .unwrap_or(std::cmp::Ordering::Equal)
        {
            std::cmp::Ordering::Equal => b.cmp(&a),
            ordering => ordering,
        }
    });

    let mut applied = 0usize;
    for &op_index in &order {
        match &operations[op_index] {
            BlockTextEditOp::Insert {
                offset,
                text,
                attrs,
                inherit_format,
            } => {
                if text.is_empty() {
                    continue;
                }
                let (segment, local_offset) =
                    locate_text_segment(&segments, *offset, length_before, true)?;
                let inherit = !matches!(inherit_format, Some(false));
                let resolved_attrs: Option<Attrs> = match attrs {
                    Some(map) => to_format_attrs(map),
                    None => {
                        if inherit && local_offset > 0.0 {
                            format_attrs_at(txn, &segment.text, local_offset)
                        } else {
                            None
                        }
                    }
                };
                let content = xml_text_content(txn, &segment.text);
                let current_len = utf16_len(&content);
                let local_units = (local_offset.max(0.0).floor() as usize).min(current_len);
                let byte_offset = utf16_to_byte_offset(&content, local_units) as u32;
                match resolved_attrs {
                    Some(format) => {
                        segment
                            .text
                            .insert_with_attributes(txn, byte_offset, text, format)
                    }
                    None => segment.text.insert(txn, byte_offset, text),
                }
                applied += 1;
            }
            BlockTextEditOp::Delete { offset, length } => {
                if *length <= 0.0 {
                    continue;
                }
                delete_text_range(txn, &segments, *offset, *length, length_before);
                applied += 1;
            }
        }
    }

    let segments_after = collect_text_segments(txn, element);
    let length_after = segments_after.iter().map(|s| s.length).sum();
    Ok(BlockTextEditResult {
        block_id,
        length_before,
        length_after,
        applied,
    })
}

/// Port of deleteRange: walk segments back-to-front removing the overlap.
fn delete_text_range(
    txn: &mut TransactionMut<'_>,
    segments: &[TextSegment],
    global_offset: f64,
    length: f64,
    total_length: usize,
) {
    let start = global_offset.max(0.0).min(total_length as f64);
    let end = (start + length).min(total_length as f64).max(start);
    if start == end {
        return;
    }
    for segment in segments.iter().rev() {
        let segment_start = segment.start as f64;
        let segment_end = (segment.start + segment.length) as f64;
        if segment_end <= start || segment_start >= end {
            continue;
        }
        let local_start = (start - segment_start).max(0.0);
        let local_end = (end - segment_start).min(segment.length as f64);
        if local_end - local_start <= 0.0 {
            continue;
        }
        let content = xml_text_content(txn, &segment.text);
        let current_len = utf16_len(&content);
        let local_start_units = (local_start.floor() as usize).min(current_len);
        let local_end_units = (local_end.floor() as usize).min(current_len);
        if local_end_units <= local_start_units {
            continue;
        }
        let byte_start = utf16_to_byte_offset(&content, local_start_units);
        let byte_end = utf16_to_byte_offset(&content, local_end_units);
        segment
            .text
            .remove_range(txn, byte_start as u32, (byte_end - byte_start) as u32);
    }
}

// ---------------------------------------------------------------------------
// Attribute updates (port of updateBlockAttributes)
// ---------------------------------------------------------------------------

/// Port of updateBlockAttributes: null clears; otherwise set unless the
/// existing primitive value already strictly equals the new one (objects and
/// arrays always rewrite — JS compared them by reference).
pub fn update_block_attributes(
    txn: &mut TransactionMut<'_>,
    element: &XmlElementRef,
    attrs: &Map<String, Value>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for (key, value) in attrs {
        if value.is_null() {
            if element.get_attribute(txn, key).is_some() {
                element.remove_attribute(txn, &key.as_str());
                changed.push(key.clone());
            }
            continue;
        }
        let primitive = matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_));
        if primitive {
            let existing = element.get_attribute(txn, key).map(|out| out_to_json(&out));
            if existing.as_ref() == Some(value) {
                continue;
            }
        }
        element.insert_attribute(txn, key.clone(), json_to_any(value));
        changed.push(key.clone());
    }
    changed
}

// ---------------------------------------------------------------------------
// Payload parsers (ports of blocksFromInsertPayload / blockEditsFromPayload /
// textEditOpsFromPayload / collectBlockIds)
// ---------------------------------------------------------------------------

/// Port of blocksFromInsertPayload.
///
/// Deviation: the TS helper swallowed content-parse failures into an empty
/// list (surfacing as "no parsable blocks"); here parse errors propagate with
/// their own message.
fn blocks_from_insert_payload(
    payload: &Map<String, Value>,
    operation_id: &str,
) -> Result<Vec<Value>, String> {
    if let Some(content) = payload.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            let parsed = super::content_parse::parse_write_content_for_operation(
                content,
                payload.get("format").and_then(Value::as_str),
                operation_id,
            )?;
            return Ok(parsed
                .tiptap_json
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default());
        }
    }
    if let Some(raw) = coalesce(payload, &["tiptapJson", "tiptap_json"]) {
        let object = obj(raw);
        let qualifies = object.get("type").map(js_truthy).unwrap_or(false)
            || object.get("content").map(Value::is_array).unwrap_or(false);
        if qualifies {
            if let Some(content) = object.get("content").and_then(Value::as_array) {
                return Ok(content.clone());
            }
        }
    }
    if let Some(blocks) = payload.get("blocks").and_then(Value::as_array) {
        return blocks
            .iter()
            .map(super::document_ops::block_to_tiptap_node)
            .collect();
    }
    Ok(Vec::new())
}

pub struct BlockUpdateEdit {
    pub block_id: String,
    pub attrs: Map<String, Value>,
}

/// Port of blockEditsFromPayload.
pub fn block_edits_from_payload(payload: &Map<String, Value>) -> Vec<BlockUpdateEdit> {
    let list = payload
        .get("edits")
        .and_then(Value::as_array)
        .or_else(|| payload.get("updates").and_then(Value::as_array));
    if let Some(list) = list {
        let mut edits = Vec::new();
        for entry in list {
            let data = obj(entry);
            let Some(block_id) = js_string_value(coalesce(&data, &["blockId", "block_id"])) else {
                continue;
            };
            let attrs = coalesce(&data, &["attrs", "attributes"])
                .map(obj)
                .unwrap_or_default();
            edits.push(BlockUpdateEdit { block_id, attrs });
        }
        return edits;
    }
    if let Some(block_id) = js_string_value(coalesce(payload, &["blockId", "block_id"])) {
        let attrs = coalesce(payload, &["attrs", "attributes"])
            .map(obj)
            .unwrap_or_default();
        return vec![BlockUpdateEdit { block_id, attrs }];
    }
    Vec::new()
}

/// Port of textEditOpsFromPayload.
pub fn text_edit_ops_from_payload(value: Option<&Value>) -> Vec<BlockTextEditOp> {
    let Some(list) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut ops = Vec::new();
    for entry in list {
        let data = obj(entry);
        let Some(op_type) = js_string_value(data.get("type")) else {
            continue;
        };
        if op_type != "insert" && op_type != "delete" {
            continue;
        }
        let Some(offset) = js_finite_number(data.get("offset")) else {
            continue;
        };
        if op_type == "insert" {
            let text = js_string_value(data.get("text")).unwrap_or_default();
            // JS: data.inheritFormat ?? data.inherit_format, then
            // `=== undefined ? undefined : Boolean(value)` — an explicit null
            // resolves to false, a missing key to None.
            let inherit_raw: Option<&Value> = match data.get("inheritFormat") {
                Some(v) if !v.is_null() => Some(v),
                _ => data.get("inherit_format"),
            };
            let inherit_format = inherit_raw.map(js_truthy);
            let attrs = data.get("attrs").map(obj);
            ops.push(BlockTextEditOp::Insert {
                offset,
                text,
                attrs,
                inherit_format,
            });
        } else {
            let Some(length) = js_finite_number(data.get("length")) else {
                continue;
            };
            ops.push(BlockTextEditOp::Delete { offset, length });
        }
    }
    ops
}

/// Port of collectBlockIds.
pub fn collect_block_ids(payload: &Map<String, Value>) -> Vec<String> {
    if let Some(ids) = payload.get("blockIds").filter(|v| v.is_array()) {
        return js_string_array(ids);
    }
    if let Some(ids) = payload.get("block_ids").filter(|v| v.is_array()) {
        return js_string_array(ids);
    }
    js_string_value(coalesce(payload, &["blockId", "block_id"]))
        .map(|id| vec![id])
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Room/persistence plumbing shared by the four handlers
// ---------------------------------------------------------------------------

enum TxnOutcome {
    Replayed(Option<Value>),
    Fresh(Value),
}

struct DocContext {
    room: Arc<super::rooms::Room>,
    graph_dir: PathBuf,
    graph_id: String,
    document_id: String,
}

fn require_document_id(operation: &CrdtOperation, kind: &str) -> Result<String, String> {
    operation
        .document_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| format!("{kind}: documentId is required"))
}

async fn document_context(
    app: &AppHandle,
    operation: &CrdtOperation,
    document_id: &str,
) -> Result<DocContext, String> {
    let graph_id = operation.graph_id.clone();
    let graph_dir = crate::graph_paths::existing_graph_dir(app, &graph_id)?;
    // F8: block-mutation is the other authoring entry point that self-heals a
    // genuine ghost document (listed in the workspace Y.Doc, no manifest on
    // disk yet). Gated exactly like the read side — pure headless cells only,
    // self-heal flag on. Unlike `read_document`, this call site never had an
    // existence check before F8 (a missing manifest was always silently
    // bootstrapped later by `save_document`'s own unconditional
    // `read_or_initialize_document_record`) — so the flag is checked BEFORE
    // touching the filesystem at all, and the whole thing is skipped when the
    // flag is off, to keep flag-disabled headless cells (e.g. bare
    // `sophia-mcp` local-backend mode) byte-identical to pre-F8 behavior.
    // Only gateway-fronted cells (`GARDEN_SELF_HEAL_GRAPHS=1`) get the
    // stricter ghost-vs-typo distinction. See
    // `document_paths::self_heal_missing_document`.
    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    if crate::runtime_config::self_heal_graphs_enabled()
        && !crate::document_paths::document_dir(&graph_dir, document_id)?
            .join("document.json")
            .is_file()
    {
        // The executor already owns the graph persistence lease. Use the
        // explicitly non-reentrant healing body so this cannot deadlock by
        // trying to acquire the same lease a second time.
        crate::document_paths::self_heal_missing_document_with_lease(
            app,
            &graph_dir,
            &graph_id,
            document_id,
        )?;
    }
    let state_path = crate::ydoc_paths::checked_document_ydoc_state_path(&graph_dir, document_id)?;
    let registry = app.state::<super::rooms::RoomRegistry>();
    let room = registry
        .get_or_create(&format!("doc:{graph_id}:{document_id}"), state_path)
        .await?;
    Ok(DocContext {
        room,
        graph_dir,
        graph_id,
        document_id: document_id.to_string(),
    })
}

/// Materialize and persist the full document after a successful mutation —
/// the headless replacement for the editor channel's debounced save. Uses the
/// existing record's title; an unknown document falls back to "Untitled",
/// mirroring transactDocument's createDocument(graphId, 'Untitled', docId).
async fn persist_document(
    app: &AppHandle,
    ctx: &DocContext,
    operation: &CrdtOperation,
) -> Result<(), String> {
    let title = crate::document_paths::document_dir(&ctx.graph_dir, &ctx.document_id)
        .ok()
        .map(|dir| dir.join("document.json"))
        .filter(|manifest| manifest.exists())
        .and_then(|manifest| {
            crate::document_record_store::read_document_record(&ctx.graph_dir, &manifest).ok()
        })
        .map(|record| record.title)
        .unwrap_or_else(|| "Untitled".to_string());

    super::document_ops::flush_room_document_if_dirty(
        app,
        &ctx.graph_id,
        &ctx.document_id,
        &title,
        &ctx.room,
        &operation.operation_id,
    )
    .await?;
    Ok(())
}

fn warn_on_hash_mismatch(kind: &str, operation_id: &str, hit: &LedgerHit, payload: &Value) {
    if let Some(expected) = &hit.payload_hash {
        let current = canonical_payload_hash(payload);
        if expected != &current {
            log::warn!(
                "[{kind}] payload hash mismatch on replay (operation {operation_id}: expected {expected}, got {current})"
            );
        }
    }
}

/// Assemble {success, replayed, ...envelope} responses. A ledger replay does
/// not repeat the hot mutation, but it still repairs a dirty cold-projection
/// tail left by a crash/failure after the mutation+ledger sidecar was durable.
async fn finish_ledger_guarded(
    app: &AppHandle,
    ctx: &DocContext,
    operation: &CrdtOperation,
    outcome: TxnOutcome,
) -> ApplyOperationResult<Value> {
    let (replayed, envelope) = match outcome {
        TxnOutcome::Replayed(cached) => {
            persist_document(app, ctx, operation)
                .await
                .map_err(ApplyOperationError::retryable_after_hot_commit)?;
            (true, cached.unwrap_or_else(|| json!({})))
        }
        TxnOutcome::Fresh(envelope) => {
            persist_document(app, ctx, operation)
                .await
                .map_err(ApplyOperationError::retryable_after_hot_commit)?;
            (false, envelope)
        }
    };
    let mut response = Map::new();
    response.insert("success".to_string(), json!(true));
    response.insert("replayed".to_string(), json!(replayed));
    if let Value::Object(fields) = envelope {
        for (key, value) in fields {
            response.insert(key, value);
        }
    }
    Ok(Value::Object(response))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Port of applyBlockInsert.
async fn block_insert(app: &AppHandle, operation: &CrdtOperation) -> ApplyOperationResult<Value> {
    let document_id = require_document_id(operation, "block.insert")?;
    let payload = obj(&operation.payload);
    let blocks = blocks_from_insert_payload(&payload, &operation.operation_id)?;
    if blocks.is_empty() {
        return Err(ApplyOperationError::terminal(
            "block.insert: no parsable blocks in payload",
        ));
    }
    let ref_block_id = js_string_value(coalesce(
        &payload,
        &["blockId", "block_id", "afterBlockId", "after_block_id"],
    ));
    let index_hint = js_finite_number(coalesce(&payload, &["index", "position"]));
    let position_before = js_string_value(payload.get("position")).as_deref() == Some("before");

    let ctx = document_context(app, operation, &document_id).await?;
    let operation_id = operation.operation_id.clone();
    let enqueue_timestamp = operation.enqueue_timestamp.clone();
    let payload_value = Value::Object(payload);

    let outcome = ctx
        .room
        .update_doc(move |_doc, txn| {
            // Replay guard: the ledger is co-written transactionally with the
            // mutation, so a hit means the prior apply was durable.
            if let Some(hit) = read_ledger_entry(txn, &operation_id) {
                warn_on_hash_mismatch("block.insert", &operation_id, &hit, &payload_value);
                return Ok(TxnOutcome::Replayed(hit.result));
            }

            let fragment = content_fragment(txn);
            let block_count = fragment.len(txn);
            let located = match &ref_block_id {
                Some(ref_id) => match find_block_in_fragment(txn, &fragment, ref_id) {
                    Some((_, index)) => Some(index),
                    None => return Err(format!("Block not found: {ref_id}")),
                },
                None => None,
            };
            let insert_at = resolve_insert_index(located, position_before, index_hint, block_count);

            // Structural pre-check: a payload block whose data-block-id is
            // already in the fragment counts as already applied (guards
            // "different operationId, same payload blockId").
            let mut existing_block_ids: Vec<String> = Vec::new();
            let mut new_blocks: Vec<Value> = Vec::new();
            // Some(id) = existing decision, None = new block (payload order).
            let mut decisions: Vec<Option<String>> = Vec::new();
            for block in &blocks {
                match read_block_id_from_json(block) {
                    Some(candidate)
                        if find_block_in_fragment(txn, &fragment, &candidate).is_some() =>
                    {
                        decisions.push(Some(candidate.clone()));
                        existing_block_ids.push(candidate);
                    }
                    _ => {
                        decisions.push(None);
                        new_blocks.push(block.clone());
                    }
                }
            }

            let inserted_ids = if new_blocks.is_empty() {
                Vec::new()
            } else {
                insert_blocks_into_fragment(txn, &fragment, &new_blocks, Some(insert_at))?
            };

            // Reconstruct ordered block_ids matching the payload sequence.
            let mut block_ids: Vec<String> = Vec::new();
            let mut insert_cursor = 0usize;
            for decision in decisions {
                match decision {
                    Some(id) => block_ids.push(id),
                    None => {
                        block_ids.push(inserted_ids[insert_cursor].clone());
                        insert_cursor += 1;
                    }
                }
            }

            let envelope = json!({
                "block_ids": block_ids,
                "blocks_inserted": inserted_ids.len(),
                "blocks_existing": existing_block_ids.len(),
                "existing_block_ids": existing_block_ids,
                "block_count": fragment.len(txn),
            });
            write_ledger_entry(
                txn,
                &operation_id,
                LedgerWriteFields {
                    op: "block.insert",
                    completed_at: &enqueue_timestamp,
                    payload_hash: Some(canonical_payload_hash(&payload_value)),
                    result: Some(&envelope),
                },
            );
            prune_ledger(txn, epoch_ms_now());
            Ok(TxnOutcome::Fresh(envelope))
        })
        .await?;

    finish_ledger_guarded(app, &ctx, operation, outcome).await
}

/// Port of applyBlockUpdate.
async fn block_update(app: &AppHandle, operation: &CrdtOperation) -> ApplyOperationResult<Value> {
    let document_id = require_document_id(operation, "block.update")?;
    let payload = obj(&operation.payload);
    let edits = block_edits_from_payload(&payload);
    if edits.is_empty() {
        return Err(ApplyOperationError::terminal(
            "block.update: no edits in payload",
        ));
    }

    let ctx = document_context(app, operation, &document_id).await?;
    let (updated, missing) = ctx
        .room
        .update_doc(move |_doc, txn| {
            let fragment = content_fragment(txn);
            let block_index = index_blocks_in_fragment(txn, &fragment);
            let mut updated: Vec<String> = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            for edit in &edits {
                match block_index.get(&edit.block_id) {
                    None => missing.push(edit.block_id.clone()),
                    Some((element, _)) => {
                        update_block_attributes(txn, element, &edit.attrs);
                        updated.push(edit.block_id.clone());
                    }
                }
            }
            Ok((updated, missing))
        })
        .await?;

    persist_document(app, &ctx, operation)
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    Ok(json!({ "success": true, "updated": updated, "missing": missing }))
}

/// Port of applyBlockEditText.
async fn block_edit_text(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let document_id = require_document_id(operation, "block.editText")?;
    let payload = obj(&operation.payload);
    let block_id = js_string_value(coalesce(&payload, &["blockId", "block_id"]))
        .ok_or_else(|| "block.editText: blockId is required".to_string())?;
    let operations = text_edit_ops_from_payload(coalesce(&payload, &["operations", "ops"]));
    if operations.is_empty() {
        return Err(ApplyOperationError::terminal(
            "block.editText: operations must not be empty",
        ));
    }

    let ctx = document_context(app, operation, &document_id).await?;
    let operation_id = operation.operation_id.clone();
    let enqueue_timestamp = operation.enqueue_timestamp.clone();
    let payload_value = Value::Object(payload);

    let outcome = ctx
        .room
        .update_doc(move |_doc, txn| {
            // Replay guard: offset-based text mutations are NOT idempotent —
            // replaying against mutated text corrupts silently. The ledger is
            // the only correct guard here.
            if let Some(hit) = read_ledger_entry(txn, &operation_id) {
                warn_on_hash_mismatch("block.editText", &operation_id, &hit, &payload_value);
                return Ok(TxnOutcome::Replayed(hit.result));
            }

            let fragment = content_fragment(txn);
            let (element, _) = find_block_in_fragment(txn, &fragment, &block_id)
                .ok_or_else(|| format!("Block not found: {block_id}"))?;
            let edit = edit_block_text(txn, &element, &operations)?;

            let envelope = json!({
                "block_id": edit.block_id,
                "length_before": edit.length_before,
                "length_after": edit.length_after,
                "applied": edit.applied,
            });
            write_ledger_entry(
                txn,
                &operation_id,
                LedgerWriteFields {
                    op: "block.editText",
                    completed_at: &enqueue_timestamp,
                    payload_hash: Some(canonical_payload_hash(&payload_value)),
                    result: Some(&envelope),
                },
            );
            prune_ledger(txn, epoch_ms_now());
            Ok(TxnOutcome::Fresh(envelope))
        })
        .await?;

    finish_ledger_guarded(app, &ctx, operation, outcome).await
}

/// Port of applyBlockDelete.
async fn block_delete(app: &AppHandle, operation: &CrdtOperation) -> ApplyOperationResult<Value> {
    let document_id = require_document_id(operation, "block.delete")?;
    let payload = obj(&operation.payload);
    let requested = collect_block_ids(&payload);
    if requested.is_empty() {
        return Err(ApplyOperationError::terminal(
            "block.delete: blockIds must not be empty",
        ));
    }

    let ctx = document_context(app, operation, &document_id).await?;
    let (deleted, missing, remaining) = ctx
        .room
        .update_doc(move |_doc, txn| {
            let fragment = content_fragment(txn);
            let block_index = index_blocks_in_fragment(txn, &fragment);
            let mut deleted: Vec<String> = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            let mut scheduled_ids: HashSet<String> = HashSet::new();
            let mut scheduled_indexes: Vec<u32> = Vec::new();
            for id in &requested {
                match block_index.get(id) {
                    Some((_, index)) if !scheduled_ids.contains(id) => {
                        scheduled_ids.insert(id.clone());
                        scheduled_indexes.push(*index);
                        deleted.push(id.clone());
                    }
                    _ => missing.push(id.clone()),
                }
            }
            scheduled_indexes.sort_unstable_by(|a, b| b.cmp(a));
            for index in scheduled_indexes {
                fragment.remove_range(txn, index, 1);
            }
            let remaining = block_ids_in_fragment(txn, &fragment);
            Ok((deleted, missing, remaining))
        })
        .await?;

    persist_document(app, &ctx, operation)
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    Ok(json!({
        "success": true,
        "deleted": deleted,
        "missing": missing,
        "remaining_block_ids": remaining,
    }))
}
