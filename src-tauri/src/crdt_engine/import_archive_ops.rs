//! graph.importArchive — whole-graph tar.gz import for headless cells.
//! Port of frontend/src/native/native-local-runtime.ts importGraphArchive
//! (~2456-2649) + frontend/src/native/graph-archive-import.ts (all of it).
//!
//! Layering:
//! - the pure layer (`parse_graph_archive`, `rewrite_graph_archive_rdf`,
//!   `rewrite_graph_archive_workspace`) ports graph-archive-import.ts —
//!   bounded gunzip + manual ustar reader (inherited member/path limits,
//!   additional whole-container and terminal-tail bounds), manifest validation, and the
//!   source→target identity rewrites for RDF and the workspace Y.Doc;
//! - `apply` orchestrates the import exactly like importGraphArchive:
//!   completion-ledger replay guard → graph-existence/partial-replay check →
//!   create graph → restore + rewrite workspace Y.Doc (save_workspace with a
//!   materialized snapshot) → per-document Y.Doc restore via the same
//!   create_document/save_document internals the desktop frontend uses →
//!   rewritten n-quads into the graph store → dual snake/camel envelope →
//!   Tier B completion-ledger append.
//!
//! Rooms note: imported document/workspace Y.Docs are written straight to
//! disk. Owned restore therefore requires no pre-existing graph rooms and
//! holds a registry admission guard through persistence. It never evicts a
//! live room; first access after completion hydrates the restored files.

use super::executor::{ApplyOperationError, ApplyOperationResult};
use crate::app_runtime::AppHandle;
use crate::crdt_queue::CrdtOperation;
#[cfg(feature = "desktop")]
use tauri::Manager;
use base64::Engine;
use flate2::read::GzDecoder;
use oxigraph::io::{RdfFormat, RdfParser};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map as JsonMap, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map as YMap, MapRef, Out, ReadTxn, StateVector, Transact, Update, WriteTxn};

#[path = "preservation_v2.rs"]
mod preservation_v2;

const TAR_BLOCK_SIZE: usize = 512;
const MAX_ARCHIVE_FILES: usize = 20_000;
const MAX_EXTRACTED_SIZE: usize = 500 * 1024 * 1024;
// Payload limit plus one header and at most one padding block per member,
// and bounded conventional terminal zero padding. This bounds *all* inflated
// bytes, including unsupported members and data after the first zero header.
const MAX_TAR_TRAILING_BYTES: usize = 64 * 1024;
const MAX_TAR_CONTAINER_BYTES: usize = MAX_EXTRACTED_SIZE
    + MAX_ARCHIVE_FILES * TAR_BLOCK_SIZE * 2 + MAX_TAR_TRAILING_BYTES;
const CELL_RESTORE_MARKER_DIR: &str = ".migration";
const CELL_RESTORE_MARKER_FILE: &str = "archive-restore-v1.json";
const CELL_RESTORE_MARKER_STORAGE_ERROR: &str = "graph.restoreArchive: marker storage unavailable:";
const CELL_RESTORE_RDF_POLICY: &str = "cloud1-main-canonical-v2";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CellArchiveRestoreMarker {
    schema_version: u32,
    rdf_policy: String,
    operation_id: String,
    target_graph_id: String,
    target_graph_incarnation: String,
    target_generation: u64,
    archive_sha256: String,
    source_graph_id: String,
    source_user_id: String,
    plan_digest: String,
    expected_document_count: usize,
    expected_rdf_triple_count: usize,
    started_at: String,
}

#[derive(Debug)]
struct CellArchiveRestoreContract {
    marker: CellArchiveRestoreMarker,
    marker_path: PathBuf,
    marker_preexisted: bool,
}

fn record_import_phase(app: &AppHandle, operation_id: &str, phase: &str, started: Instant) {
    crate::crdt_queue::record_crdt_phase(app, Some(operation_id), phase, started.elapsed());
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure layer — port of frontend/src/native/graph-archive-import.ts
// ─────────────────────────────────────────────────────────────────────────────

/// Validated manifest identity (manifest.json, version 1,
/// format "mnemosyne-graph-export").
#[derive(Debug, Clone)]
pub struct GraphArchiveManifest {
    pub source_user_id: String,
    pub source_graph_id: String,
    pub source_graph_title: Option<String>,
    pub source_graph_description: Option<String>,
    pub includes_artifacts: bool,
}

/// Port of ParsedGraphArchive. `documents` preserves tar entry order
/// (the TS Map iteration order).
#[derive(Debug)]
pub struct ParsedGraphArchive {
    pub manifest: GraphArchiveManifest,
    pub rdf_n_quads: String,
    pub workspace_bytes: Option<Vec<u8>>,
    pub documents: Vec<(String, Vec<u8>)>,
    pub warnings: Vec<String>,
}

/// Port of parseGraphArchive.
pub fn parse_graph_archive(tar_gz_bytes: &[u8]) -> Result<ParsedGraphArchive, String> {
    require_compressed_archive_size(tar_gz_bytes.len())?;
    let tar_bytes = read_gzip_bounded(tar_gz_bytes, MAX_TAR_CONTAINER_BYTES)?;

    let entries = read_tar_entries(&tar_bytes)?;
    let manifest_bytes = entries_get(&entries, "manifest.json")
        .ok_or_else(|| "Cannot read manifest.json from archive".to_string())?;

    let manifest_value: Value = serde_json::from_str(&String::from_utf8_lossy(manifest_bytes))
        .map_err(|_| "Cannot parse manifest.json from archive".to_string())?;
    let manifest = parse_manifest(&manifest_value)?;

    let files = manifest_value
        .get("files")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let rdf_path = normalize_archive_path(files.get("rdf").and_then(Value::as_str).unwrap_or(""));
    let workspace_path =
        normalize_archive_path(files.get("workspace").and_then(Value::as_str).unwrap_or(""));
    let documents_dir = normalize_archive_prefix(
        files
            .get("documents_dir")
            .and_then(Value::as_str)
            .unwrap_or("crdt/documents/"),
    );

    let rdf_n_quads = if rdf_path.is_empty() {
        String::new()
    } else {
        entries_get(&entries, &rdf_path)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default()
    };
    let workspace_bytes = if workspace_path.is_empty() {
        None
    } else {
        entries_get(&entries, &workspace_path).map(<[u8]>::to_vec)
    };

    let mut documents: Vec<(String, Vec<u8>)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (entry_path, bytes) in &entries {
        if !entry_path.starts_with(&documents_dir) || !entry_path.ends_with(".yjs") {
            continue;
        }
        let filename = entry_path[documents_dir.len()..]
            .split('/')
            .next_back()
            .unwrap_or("");
        let document_id = filename.strip_suffix(".yjs").unwrap_or("");
        if document_id.is_empty() {
            warnings.push(format!(
                "Skipped document with invalid archive path: {entry_path}"
            ));
            continue;
        }
        // Map.set semantics: duplicates overwrite in place.
        if let Some(slot) = documents.iter_mut().find(|(id, _)| id == document_id) {
            slot.1 = bytes.clone();
        } else {
            documents.push((document_id.to_string(), bytes.clone()));
        }
    }

    if workspace_bytes.is_none() {
        warnings.push("Archive does not contain a workspace Y.Doc".to_string());
    }
    if documents.is_empty() {
        warnings.push("Archive does not contain document Y.Docs".to_string());
    }

    Ok(ParsedGraphArchive {
        manifest,
        rdf_n_quads,
        workspace_bytes,
        documents,
        warnings,
    })
}

fn require_compressed_archive_size(size: usize) -> Result<(), String> {
    if size > MAX_EXTRACTED_SIZE {
        return Err("Compressed archive, including its gzip header, exceeds 500 MiB".into());
    }
    Ok(())
}

fn read_gzip_bounded(bytes: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut decoder = GzDecoder::new(bytes);
    let mut result = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        // Read at most the remaining budget and a single overflow witness.
        let read_limit = (limit - result.len()).saturating_add(1).min(chunk.len());
        let count = decoder.read(&mut chunk[..read_limit])
            .map_err(|_| "Invalid or corrupt archive: expected gzip-compressed tar data".to_string())?;
        if count == 0 { return Ok(result); }
        if count > limit - result.len() {
            return Err(format!("Decompressed tar container exceeds {limit} bytes"));
        }
        result.try_reserve_exact(count).map_err(|error| format!("Cannot allocate bounded archive buffer: {error}"))?;
        result.extend_from_slice(&chunk[..count]);
    }
}

/// Manifest validation — error strings verbatim from parseGraphArchive.
fn parse_manifest(value: &Value) -> Result<GraphArchiveManifest, String> {
    let version = value.get("version");
    // JS strict `manifest.version !== 1`: only the number 1 (incl. 1.0) passes.
    if version.and_then(Value::as_f64) != Some(1.0) {
        return Err(format!(
            "Unsupported manifest version: {}",
            js_display(version)
        ));
    }
    let format = value.get("format");
    if format.and_then(Value::as_str) != Some("mnemosyne-graph-export") {
        return Err(format!("Unknown archive format: {}", js_display(format)));
    }
    let source_user_id = value.get("source_user_id");
    let source_graph_id = value.get("source_graph_id");
    if !js_truthy(source_user_id) || !js_truthy(source_graph_id) {
        return Err("Archive manifest is missing source user or graph identity".to_string());
    }
    Ok(GraphArchiveManifest {
        source_user_id: js_display(source_user_id),
        source_graph_id: js_display(source_graph_id),
        source_graph_title: nullable_string_value(value.get("source_graph_title")),
        source_graph_description: nullable_string_value(value.get("source_graph_description")),
        includes_artifacts: js_truthy(value.get("includes_artifacts")),
    })
}

/// Port of rewriteGraphArchiveRdf: source graph URN → local graph URN, plus
/// the user/graph storage-path forms (`users/{u}/` → `users/default/`,
/// `graphs/{g}/` → `graphs/{newId}/`).
pub fn rewrite_graph_archive_rdf(
    rdf_n_quads: &str,
    manifest: &GraphArchiveManifest,
    target_graph_id: &str,
) -> String {
    let source_graph_uri = format!(
        "urn:mnemosyne:user:{}:graph:{}",
        manifest.source_user_id, manifest.source_graph_id
    );
    let target_graph_uri = format!("urn:mnemosyne:local:graph:{target_graph_id}");
    rdf_n_quads
        .replace(&source_graph_uri, &target_graph_uri)
        .replace(
            &format!("users/{}/", manifest.source_user_id),
            "users/default/",
        )
        .replace(
            &format!("graphs/{}/", manifest.source_graph_id),
            &format!("graphs/{target_graph_id}/"),
        )
}

/// Port of rewriteGraphArchiveWorkspace: one transaction rewriting
/// `storageKey` on every artifact Y.Map and `sf_storageKey` on every
/// document Y.Map (sourceUserId → 'default', sourceGraphId → newGraphId).
pub fn rewrite_graph_archive_workspace(
    doc: &Doc,
    manifest: &GraphArchiveManifest,
    target_graph_id: &str,
) {
    let source_user_path = format!("users/{}/", manifest.source_user_id);
    let target_user_path = "users/default/";
    let source_graph_path = format!("graphs/{}/", manifest.source_graph_id);
    let target_graph_path = format!("graphs/{target_graph_id}/");

    let mut txn = doc.transact_mut();
    let artifacts = txn.get_or_insert_map("artifacts");
    let documents = txn.get_or_insert_map("documents");
    for (root, key) in [(artifacts, "storageKey"), (documents, "sf_storageKey")] {
        let children: Vec<MapRef> = root
            .iter(&txn)
            .filter_map(|(_, out)| match out {
                Out::YMap(child) => Some(child),
                _ => None,
            })
            .collect();
        for child in children {
            let Some(Out::Any(Any::String(value))) = child.get(&txn, key) else {
                continue;
            };
            let next = value
                .replace(&source_user_path, target_user_path)
                .replace(&source_graph_path, &target_graph_path);
            if next != *value {
                child.insert(&mut txn, key, next);
            }
        }
    }

    // Wires embed the graph id in `targetGraphId` (and `sceneGraphId`): an
    // intra-graph wire stores the graph's OWN id as its target. The
    // storageKey rewrite above never touched the wires map, so before this
    // fix every intra-graph wire in an imported graph dangled at the old
    // graph id — clicking one asked for a graph that no longer exists
    // (`rdf_dump: graph not found: default`, 2026-09-03). Only an EXACT match
    // of the source graph id is rewritten (→ the new graph id, making the
    // wire resolve locally again). Wires targeting a DIFFERENT id are left
    // untouched: cross-graph wires are anomalies (the hard-boundary rule was
    // not always explicit), not a supported feature — they are not made to
    // work here, they only need to fail gracefully at click time, which is a
    // separate frontend concern.
    let wires = txn.get_or_insert_map("wires");
    let wire_children: Vec<MapRef> = wires
        .iter(&txn)
        .filter_map(|(_, out)| match out {
            Out::YMap(child) => Some(child),
            _ => None,
        })
        .collect();
    for wire in wire_children {
        for field in ["targetGraphId", "sceneGraphId"] {
            let Some(Out::Any(Any::String(value))) = wire.get(&txn, field) else {
                continue;
            };
            if value.as_ref() == manifest.source_graph_id {
                wire.insert(&mut txn, field, target_graph_id.to_string());
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// tar reader — port of readTarEntries with inherited member/path rules.
// Safety tightening: nonzero or oversized bytes after the first zero header,
// and incomplete nonzero trailing headers, are explicitly refused instead of
// silently ignored. Missing end markers at an exact member boundary remain
// compatible; gzip first-member decoding remains the inherited behavior.
// ─────────────────────────────────────────────────────────────────────────────

fn read_tar_entries(tar_bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut offset = 0usize;
    let mut count = 0usize;
    let mut extracted_size = 0usize;

    while offset + TAR_BLOCK_SIZE <= tar_bytes.len() {
        let header = &tar_bytes[offset..offset + TAR_BLOCK_SIZE];
        if header.iter().all(|byte| *byte == 0) {
            let tail = &tar_bytes[offset..];
            if tail.len() > MAX_TAR_TRAILING_BYTES || tail.iter().any(|byte| *byte != 0) {
                return Err("Invalid tar tail: expected at most 64 KiB of zero padding".into());
            }
            return Ok(entries);
        }

        count += 1;
        if count > MAX_ARCHIVE_FILES {
            return Err(format!(
                "Archive contains too many files (>{MAX_ARCHIVE_FILES})"
            ));
        }

        let name = tar_string(header, 0, 100);
        let prefix = tar_string(header, 345, 155);
        let path = normalize_archive_path(&if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        });
        validate_archive_path(&path)?;
        let size = tar_octal(header, 124, 12)?;
        let typeflag = header[156];
        let data_offset = offset + TAR_BLOCK_SIZE;
        let padded_size = size.div_ceil(TAR_BLOCK_SIZE).checked_mul(TAR_BLOCK_SIZE)
            .ok_or("Invalid tar member size overflow")?;
        let next_offset = data_offset.checked_add(padded_size).ok_or("Invalid tar offset overflow")?;
        if next_offset > tar_bytes.len() {
            return Err(format!(
                "Invalid or corrupt archive: truncated entry {path}"
            ));
        }

        extracted_size = extracted_size.checked_add(size).ok_or("Invalid extracted size overflow")?;
        if extracted_size > MAX_EXTRACTED_SIZE {
            return Err(format!(
                "Archive too large. Maximum size: {}MB",
                MAX_EXTRACTED_SIZE / (1024 * 1024)
            ));
        }

        if typeflag == 0 || typeflag == b'0' {
            let bytes = tar_bytes[data_offset..data_offset + size].to_vec();
            if let Some(slot) = entries.iter_mut().find(|(existing, _)| *existing == path) {
                slot.1 = bytes;
            } else {
                entries.push((path, bytes));
            }
        }
        offset = next_offset;
    }

    if tar_bytes[offset..].iter().any(|byte| *byte != 0) {
        return Err("Invalid tar tail: incomplete nonzero header".into());
    }

    Ok(entries)
}

fn entries_get<'a>(entries: &'a [(String, Vec<u8>)], path: &str) -> Option<&'a [u8]> {
    entries
        .iter()
        .find(|(entry_path, _)| entry_path == path)
        .map(|(_, bytes)| bytes.as_slice())
}

/// Port of tarString: NUL-terminated field, UTF-8 decoded, trimmed.
fn tar_string(block: &[u8], start: usize, length: usize) -> String {
    let slice = &block[start..start + length];
    let end = slice
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).trim().to_string()
}

/// Port of tarOctal: parseInt(raw, 8) semantics (leading octal digits, an
/// optional sign; anything unparsable or negative is an error).
fn tar_octal(block: &[u8], start: usize, length: usize) -> Result<usize, String> {
    let raw = tar_string(block, start, length).replace('\0', "");
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        return Ok(0);
    }
    let (negative, digits) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw.strip_prefix('+').unwrap_or(&raw)),
    };
    let mut value: usize = 0;
    let mut any = false;
    for ch in digits.chars() {
        match ch.to_digit(8) {
            Some(digit) => {
                value = value
                    .checked_mul(8)
                    .and_then(|v| v.checked_add(digit as usize))
                    .ok_or_else(|| format!("Invalid tar entry size: {raw}"))?;
                any = true;
            }
            None => break,
        }
    }
    if !any || negative {
        return Err(format!("Invalid tar entry size: {raw}"));
    }
    Ok(value)
}

/// Port of normalizeArchivePrefix.
fn normalize_archive_prefix(value: &str) -> String {
    let normalized = normalize_archive_path(value);
    if normalized.ends_with('/') {
        normalized
    } else {
        format!("{normalized}/")
    }
}

/// Port of normalizeArchivePath: backslashes → '/', strip one leading
/// "./"-run, collapse slash runs.
fn normalize_archive_path(value: &str) -> String {
    let replaced = value.replace('\\', "/");
    // /^\.\/+/: a leading "." followed by one or more "/".
    let stripped = match replaced.strip_prefix('.') {
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/'),
        _ => replaced.as_str(),
    };
    // /\/+/g → "/"
    let mut out = String::with_capacity(stripped.len());
    let mut previous_was_slash = false;
    for ch in stripped.chars() {
        if ch == '/' {
            if !previous_was_slash {
                out.push(ch);
            }
            previous_was_slash = true;
        } else {
            out.push(ch);
            previous_was_slash = false;
        }
    }
    out
}

/// Port of validateArchivePath: reject empty, absolute, and `..` segments.
fn validate_archive_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') {
        return Err(format!("Unsafe path in archive: {path}"));
    }
    if path.split('/').any(|part| part == "..") {
        return Err(format!("Unsafe path in archive: {path}"));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// JS payload coercion helpers (objectPayload / stringValue /
// nullableStringValue and the `a ?? b` access chains)
// ─────────────────────────────────────────────────────────────────────────────

fn obj(value: &Value) -> JsonMap<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// `map.a ?? map.b ?? ...` — `??` only skips null/undefined.
fn pick<'a>(map: &'a JsonMap<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        match map.get(*key) {
            Some(Value::Null) | None => continue,
            Some(value) => return Some(value),
        }
    }
    None
}

/// stringValue / nullableStringValue: undefined/null/'' → None, else String(v).
fn nullable_string_value(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(js_display(Some(other))),
    }
}

/// JS String() coercion for the values that can plausibly appear here.
fn js_display(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e21 {
                format!("{}", f as i64)
            } else {
                f.to_string()
            }
        }
        Some(other) => other.to_string(),
    }
}

/// JS truthiness for serde_json values (missing key counts as undefined).
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// Port of decodeBytesBase64 (atob).
fn decode_bytes_base64(value: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| format!("decode graph archive base64: {error}"))
}

fn encode_bytes_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn encode_full_state(doc: &Doc) -> Vec<u8> {
    let txn = doc.transact();
    txn.encode_state_as_update_v1(&StateVector::default())
}

fn apply_full_update(doc: &Doc, bytes: &[u8], what: &str) -> Result<(), String> {
    let update =
        Update::decode_v1(bytes).map_err(|error| format!("decode {what} Y.Doc update: {error}"))?;
    let mut txn = doc.transact_mut();
    txn.apply_update(update)
        .map_err(|error| format!("apply {what} Y.Doc update: {error}"))
}

fn required_payload_string(
    payload: &JsonMap<String, Value>,
    keys: &[&str],
    label: &str,
) -> Result<String, String> {
    nullable_string_value(pick(payload, keys))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("graph.restoreArchive: {label} is required"))
}

fn required_payload_usize(
    payload: &JsonMap<String, Value>,
    keys: &[&str],
    label: &str,
) -> Result<usize, String> {
    let value = pick(payload, keys);
    value
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("graph.restoreArchive: {label} must be a non-negative integer"))
}

fn required_payload_u64(
    payload: &JsonMap<String, Value>,
    keys: &[&str],
    label: &str,
) -> Result<u64, String> {
    pick(payload, keys)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("graph.restoreArchive: {label} must be a non-negative integer"))
}

fn require_lower_sha256(value: &str, label: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(format!(
            "graph.restoreArchive: {label} must be exactly 64 lowercase hexadecimal characters"
        ))
    }
}

fn archive_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// Bind semantic intent, not a transient upload path or a retry's wall clock.
fn restore_envelope_hash(app: &AppHandle, operation: &CrdtOperation) -> Result<String, String> {
    let payload = obj(&operation.payload);
    let boundary = app.try_state::<std::sync::Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .ok_or("graph.restoreArchive: managed cell boundary is required")?;
    boundary.authorize_crdt_operation(&operation.kind, &operation.graph_id, &operation.payload)
        .map_err(|error| format!("graph.restoreArchive: {error:?}"))?;
    if boundary.owner_graph_id() != Some(operation.graph_id.as_str())
        || boundary.graph_generation() != payload.get("targetGeneration").and_then(Value::as_u64)
        || boundary.owner_principal().and_then(|owner| owner.strip_prefix("user:"))
            != payload.get("sourceUserId").and_then(Value::as_str) {
        return Err("graph.restoreArchive: envelope does not match the cell binding".into());
    }
    let mut intent = JsonMap::new();
    for key in ["newGraphId", "archiveSha256", "sourceGraphId", "sourceUserId", "targetGeneration",
        "planDigest", "expectedDocumentCount", "expectedRdfTripleCount", "includesArtifacts", "graphIncarnation"] {
        intent.insert(key.into(), payload.get(key).cloned().unwrap_or(Value::Null));
    }
    intent.insert("operationId".into(), json!(operation.operation_id));
    intent.insert("graphId".into(), json!(operation.graph_id));
    intent.insert("rdfPolicy".into(), json!(CELL_RESTORE_RDF_POLICY));
    if let Some(version) = payload.get("formatVersion") {
        intent.insert("formatVersion".into(), version.clone());
    }
    // Insert only when present, exactly as formatVersion does: adding an unconditional key
    // would fold a null into every historical envelope and change every existing hash.
    // Covered here so a conceded admission standard cannot be added to a signed plan
    // after the fact without changing the envelope.
    if let Some(parity) = payload.get("contentParity") {
        intent.insert("contentParity".into(), parity.clone());
    }
    Ok(archive_sha256(&serde_json::to_vec(&intent).map_err(|error| error.to_string())?))
}

struct CellArchivePreflight {
    user_rdf: String,
    regenerated_statements: usize,
    authored_statements: usize,
    legacy_timestamp_normalizations: Vec<Value>,
    retained_derived_assertions: Vec<Value>,
}

/// Pure preflight. No target graph, room, marker or RDF store is opened here.
/// Projection testimony is consumed only when the full mapped RDF statement
/// exactly agrees with the native projector's output from archived Y.Doc state.
fn preflight_cell_archive(parsed: &ParsedGraphArchive, target: &str) -> Result<CellArchivePreflight, String> {
    preflight_cell_archive_with_legacy_timestamps(parsed, target, false, &HashMap::new())
}

fn preflight_cell_archive_with_legacy_timestamps(parsed: &ParsedGraphArchive, target: &str, legacy_timestamps: bool, original_assertions: &HashMap<String,Value>) -> Result<CellArchivePreflight, String> {
    preflight_cell_archive_with_availability(parsed, target, legacy_timestamps, original_assertions, &std::collections::BTreeSet::new())
}

fn preflight_cell_archive_with_availability(parsed: &ParsedGraphArchive, target: &str, legacy_timestamps: bool, original_assertions: &HashMap<String,Value>, unavailable: &std::collections::BTreeSet<String>) -> Result<CellArchivePreflight, String> {
    preflight_cell_archive_with_source_evidence(parsed,target,legacy_timestamps,original_assertions,unavailable,None)
}

fn preflight_cell_archive_with_source_evidence(parsed: &ParsedGraphArchive, target: &str, legacy_timestamps: bool, original_assertions: &HashMap<String,Value>, unavailable: &std::collections::BTreeSet<String>, evidence_archive_sha256: Option<&str>) -> Result<CellArchivePreflight, String> {
    preflight_cell_archive_with_content_parity(parsed, target, legacy_timestamps, original_assertions,
        unavailable, evidence_archive_sha256, &super::content_parity::Concessions::none())
}

fn preflight_cell_archive_with_content_parity(parsed: &ParsedGraphArchive, target: &str, legacy_timestamps: bool, original_assertions: &HashMap<String,Value>, unavailable: &std::collections::BTreeSet<String>, evidence_archive_sha256: Option<&str>, concessions: &super::content_parity::Concessions) -> Result<CellArchivePreflight, String> {
    use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode, Term};
    use std::collections::HashSet;
    use super::content_parity as parity;
    let manifest = &parsed.manifest;
    let source = format!("urn:mnemosyne:user:{}:graph:{}", manifest.source_user_id, manifest.source_graph_id);
    let destination = crate::rdf::graph_subject(target);
    let user_graph = NamedNode::new(crate::rdf_authority::user_rdf_graph_iri(target)).map_err(|error| error.to_string())?;
    let mut document_ids = HashSet::new();
    for (id, bytes) in &parsed.documents {
        crate::ids::validate_local_id(id, "document_id")?;
        let doc = Doc::new();
        apply_full_update(&doc, bytes, "document preflight")?;
        let _ = super::projection::materialize_ydoc(&doc, id);
        document_ids.insert(id.as_str());
    }
    for id in unavailable {
        crate::ids::validate_local_id(id, "unavailable document_id")?;
        if !document_ids.insert(id.as_str()) {
            return Err("unavailable document overlaps a present body".into());
        }
    }
    let mut order_authorities = HashMap::new();
    let mut absent_access_authorities = HashMap::new();
    let mut scalar_authorities = HashMap::new();
    let snapshot = if let Some(bytes) = &parsed.workspace_bytes {
        let workspace = Doc::new();
        apply_full_update(&workspace, bytes, "workspace preflight")?;
        if legacy_timestamps {
            let txn = workspace.transact();
            for (root,kind) in [("documents","document"),("folders","folder"),("artifacts","artifact"),("wires","wire")] {
                let Some(rows) = txn.get_map(root) else { continue; };
                for (id,row) in rows.iter(&txn) {
                    let Out::YMap(row) = row else { continue; };
                    let subject = if kind == "document" { crate::rdf::document_subject(id) }
                        else { format!("{}:{kind}:{id}",crate::rdf::graph_subject(target)) };
                    let mut fields=serde_json::Map::new();
                    for key in ["title","updatedAt","createdAt","deletedAt","snapshotAt","sourceSnippet","targetSnippet","lastAccessedAt","inverseOf","bidirectional","readOnly",
                        "sourceStorageKey","sourceOriginalFilename","sourceMimeType","sourceFileType","sourceContentSize"] {
                        let (presence,value)=match row.get(&txn,key) {
                            None => ("absent",Value::Null),
                            Some(Out::Any(Any::Null)) => ("null",Value::Null),
                            Some(Out::Any(Any::String(v))) => ("present",json!(v.as_ref())),
                            Some(Out::Any(Any::Number(v))) if v.is_finite() => ("present",json!(v)),
                            Some(Out::Any(Any::Bool(v))) => ("present",json!(v)),
                            _ => continue,
                        };
                        fields.insert(key.into(),json!({"root":root,"entityId":id,"key":key,"presence":presence,"value":value}));
                    }
                    scalar_authorities.insert(subject.clone(),Value::Object(fields));
                    if root=="wires" { continue; }
                    if row.get(&txn,"lastAccessedAt").is_none() {
                        absent_access_authorities.insert(subject.clone(),json!({"root":root,"entityId":id,"key":"lastAccessedAt","presence":"absent"}));
                    }
                    let Some(Out::Any(Any::Number(value))) = row.get(&txn,"order") else { continue; };
                    if !value.is_finite() { continue; }
                    order_authorities.insert(subject,json!({"root":root,"entityId":id,"key":"order","value":value}));
                }
            }
        }
        rewrite_graph_archive_workspace(&workspace, manifest, target);
        super::workspace_ops::materialize_workspace_snapshot_json(target, &workspace)?
    } else { json!({}) };
    let mut workspace_titles = HashMap::new();
    if let Some(entries) = snapshot.get("documents").and_then(Value::as_array) {
        for entry in entries {
            if let (Some(id), Some(title)) = (entry.get("id").and_then(Value::as_str), entry.get("title").and_then(Value::as_str)) {
                workspace_titles.insert(id, title);
            }
        }
    }
    if unavailable.iter().any(|id| !workspace_titles.contains_key(id.as_str())) {
        return Err("unavailable document absent from workspace metadata".into());
    }
    for (id, _) in &parsed.documents {
        let title = workspace_titles.get(id.as_str()).copied().unwrap_or(id.as_str());
        let normalized = crate::ids::normalize_stored_title(title)
            .map_err(|error| format!("graph.restoreArchive: unsupported source document title for {id}: {error}"))?;
        if normalized != title {
            return Err(format!("graph.restoreArchive: unsupported source document title for {id}: exact preservation would require normalization"));
        }
    }
    let projected = crate::rdf_workspace_materializer::workspace_entity_triples(target, &snapshot)
        .iter().map(crate::rdf::format_rdf_triple).collect::<Vec<_>>().join("\n");
    let mut projection_statements = HashSet::new();
    let mut projection_subjects = HashSet::new();
    for quad in RdfParser::from_format(RdfFormat::NTriples).for_slice(projected.as_bytes()) {
        let quad = quad.map_err(|error| error.to_string())?;
        projection_subjects.insert(quad.subject.to_string());
        projection_statements.insert(format!("{} {} {}", quad.subject, quad.predicate, quad.object));
    }
    let evidence_wires = if legacy_timestamps && evidence_archive_sha256.is_some() {
        legacy_source_only_wire_subjects(&parsed.rdf_n_quads,&snapshot,&projection_subjects,
            &manifest.source_user_id,&manifest.source_graph_id,target)?
    } else { std::collections::HashSet::new() };
    let evidence_graph = evidence_archive_sha256.map(|sha|legacy_source_evidence_graph(
        &manifest.source_user_id,&manifest.source_graph_id,target,sha,&source));
    let parity_graph = evidence_archive_sha256.map(|sha|content_parity_evidence_graph(
        &manifest.source_user_id,&manifest.source_graph_id,target,sha,&source));
    // A source document reference the archive cannot account for: neither a present
    // body nor a declared unavailable one. Strictly terminal, because `rewrite` has no
    // native subject to produce; conceded, retained as source evidence with the
    // dangling id recorded. Checked before the rewrite so the quad is still intact.
    let unresolved_doc = |node: &NamedNode| -> Option<String> {
        node.as_str().strip_prefix(&format!("{source}:doc:"))
            .map(|document| document.split_once('#').map_or(document, |(id, _)| id).to_string())
            .filter(|id| !document_ids.contains(id.as_str()))
    };
    let rewrite = |node: NamedNode| -> Result<NamedNode, String> {
        if let Some(document) = node.as_str().strip_prefix(&format!("{source}:doc:")) {
            let (id, fragment) = document.split_once('#').map(|(id, tail)| (id, format!("#{tail}")))
                .unwrap_or((document, String::new()));
            if !document_ids.contains(id) {
                return Err(format!("graph.restoreArchive: unresolved source document identity {id}"));
            }
            return NamedNode::new(format!("{}{fragment}", crate::rdf::document_subject(id))).map_err(|error| error.to_string());
        }
        if let Some(suffix) = node.as_str().strip_prefix(&source) {
            if suffix.is_empty() || suffix.starts_with(':') || suffix.starts_with('/') || suffix.starts_with('#') {
                return NamedNode::new(format!("{destination}{suffix}")).map_err(|error| error.to_string());
            }
        }
        Ok(node)
    };
    // This is an existence check over already admitted original testimony. Keep
    // the exact lexical comparison, but build it once rather than formatting
    // every captured original for every authored source statement.
    let captured_original_subjects: HashSet<String> = original_assertions.values()
        .filter_map(|row| row["subject"].as_str())
        .map(|subject| format!("<{subject}>"))
        .collect();
    let mut result = CellArchivePreflight { user_rdf: String::new(), regenerated_statements: 0, authored_statements: 0, legacy_timestamp_normalizations: Vec::new(), retained_derived_assertions: Vec::new() };
    let mut projection_conflicts = std::collections::BTreeMap::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(parsed.rdf_n_quads.as_bytes()) {
        let mut quad = quad.map_err(|error| error.to_string())?;
        if !matches!(&quad.graph_name, GraphName::NamedNode(name) if name.as_str() == source) {
            return Err("graph.restoreArchive: only the exact source main RDF graph is admitted by this core contract".into());
        }
        let source_quad_canonical_sha256 = archive_sha256(format!("{quad} .\n").as_bytes());
        if matches!(&quad.subject,NamedOrBlankNode::NamedNode(node) if evidence_wires.contains(node.as_str())) {
            let source_quad=format!("{quad} .\n");
            let graph=evidence_graph.as_ref().ok_or("source evidence context missing")?;
            result.retained_derived_assertions.push(json!({
                "subject":quad.subject.to_string().trim_start_matches('<').trim_end_matches('>'),
                "predicate":quad.predicate.as_str(),"sourceQuad":source_quad,
                "sourceQuadCanonicalSha256":source_quad_canonical_sha256,
                "sourceGraphIri":source,"sourceArchiveSha256":evidence_archive_sha256,
                "source":{"quad":source_quad},"native":null,"evidenceGraph":graph,
                "authority":"captured-source-rdf-and-workspace-absence",
                "reason":"source-only-wire-anatomy-v1","disposition":"retained-queryable-source-evidence",
                "referenceOnly":true,"currentEntityExistenceAsserted":false,"fetchAuthority":false,
                "rawSourceRetained":true}));
            quad.graph_name=NamedNode::new(graph).map_err(|e|e.to_string())?.into();
            result.user_rdf.push_str(&format!("{quad} .\n"));
            continue;
        }
        let dangling = [
            match &quad.subject { NamedOrBlankNode::NamedNode(node) => unresolved_doc(node), _ => None },
            unresolved_doc(&quad.predicate),
            match &quad.object { Term::NamedNode(node) => unresolved_doc(node), _ => None },
        ].into_iter().flatten().next();
        if let Some(id) = dangling {
            let terminal = || format!("graph.restoreArchive: unresolved source document identity {id}");
            if !concessions.allows(parity::UNRESOLVED_SOURCE_DOCUMENT_IDENTITY) { return Err(terminal()); }
            // No evidence graph means nowhere to retain it, and a concession that dropped
            // content would invert the meaning of content parity. Strict error stands.
            let graph = parity_graph.as_ref().ok_or_else(terminal)?;
            let source_quad = format!("{quad} .\n");
            let mut row = parity::disposition(parity::UNRESOLVED_SOURCE_DOCUMENT_IDENTITY,
                "unresolved-source-document-identity-v1",
                quad.subject.to_string().trim_start_matches('<').trim_end_matches('>'),
                quad.predicate.as_str(), &source_quad, &source_quad_canonical_sha256,
                &source, evidence_archive_sha256, graph, Value::Null);
            row["unresolvedDocumentId"] = json!(id);
            result.retained_derived_assertions.push(row);
            quad.graph_name = NamedNode::new(graph).map_err(|e|e.to_string())?.into();
            result.user_rdf.push_str(&format!("{quad} .\n"));
            continue;
        }
        if let NamedOrBlankNode::NamedNode(node) = quad.subject { quad.subject = rewrite(node)?.into(); }
        quad.predicate = rewrite(quad.predicate)?;
        if let Term::NamedNode(node) = &quad.object {
            let mapped = rewrite(node.clone());
            let already_exact = mapped.as_ref().ok().is_some_and(|object|
                projection_statements.contains(&format!("{} {} {}",quad.subject,quad.predicate,object)));
            if legacy_timestamps && !already_exact {
                if let Some(mut evidence) = legacy_saved_wire_endpoint(&quad,&snapshot,
                    &projection_statements,&manifest.source_user_id,&manifest.source_graph_id,target) {
                    evidence["sourceQuadCanonicalSha256"] = json!(source_quad_canonical_sha256);
                    result.legacy_timestamp_normalizations.push(evidence);
                    result.regenerated_statements += 1;
                    // The entire mapped statement is already emitted by the
                    // saved wire. No document, endpoint, grant or new RDF row.
                    continue;
                }
            }
            quad.object = mapped?.into();
        }
        let statement = format!("{} {} {}", quad.subject, quad.predicate, quad.object);
        if projection_statements.contains(&statement) {
            result.regenerated_statements += 1;
            continue;
        }
        // v2.3 only: independently emitted workspace projection may differ in
        // datatype or exact Cloud-1 timestamp-emitter representation. The named
        // rule records both values; it does not alter the captured workspace.
        if let Some(normalization) = legacy_timestamps.then(|| legacy_projection_normalization(&quad, &projection_statements)).flatten() {
            result.regenerated_statements += 1;
            result.legacy_timestamp_normalizations.push(normalization);
            continue;
        }
        if let Some(normalization) = legacy_timestamps.then(|| legacy_boolean_normalization(&quad,&projection_statements,&scalar_authorities)).flatten() {
            result.regenerated_statements += 1;
            result.legacy_timestamp_normalizations.push(normalization);
            continue;
        }
        if let Some(normalization) = legacy_timestamps.then(|| legacy_epoch_decimal_normalization(&quad,&projection_statements,&scalar_authorities)).flatten() {
            result.regenerated_statements += 1;
            result.legacy_timestamp_normalizations.push(normalization);
            continue;
        }
        if let Some(mut disposition) = legacy_timestamps.then(|| legacy_order_disposition(&quad,&projection_statements,&order_authorities)).flatten() {
            disposition["sourceQuadCanonicalSha256"] = json!(source_quad_canonical_sha256);
            result.retained_derived_assertions.push(disposition);
            // Retained in raw source custody, NOT counted as equivalent,
            // regenerated testimony or newly authored current RDF.
            continue;
        }
        if let Some(mut disposition) = legacy_timestamps.then(|| legacy_absent_access_disposition(&quad,&projection_statements,&absent_access_authorities)).flatten() {
            disposition["sourceQuadCanonicalSha256"] = json!(source_quad_canonical_sha256);
            result.retained_derived_assertions.push(disposition);
            continue;
        }
        if let Some(mut disposition) = legacy_timestamps.then(|| legacy_saved_title_disposition(&quad,&projection_statements,&scalar_authorities)).flatten() {
            disposition["sourceQuadCanonicalSha256"] = json!(source_quad_canonical_sha256);
            result.retained_derived_assertions.push(disposition);
            continue;
        }
        if let Some(mut disposition) = legacy_timestamps.then(|| legacy_saved_scalar_disposition(&quad,&projection_statements,&scalar_authorities)).flatten() {
            disposition["sourceQuadCanonicalSha256"] = json!(source_quad_canonical_sha256);
            result.retained_derived_assertions.push(disposition);
            continue;
        }
        if let Some(disposition) = original_assertions.get(&statement).filter(|_|legacy_timestamps) {
            let prefix=format!("{} {} ",quad.subject,quad.predicate);
            if disposition["rule"]=="owned-artifact-storage-key-rewrite-v1" {
                if !legacy_owned_artifact_projection(disposition,&quad,&projection_statements) {
                    return Err("graph.restoreArchive: owned artifact storage key differs from native projection".into());
                }
                let mut normalization=disposition.clone();
                normalization["sourceQuadCanonicalSha256"]=json!(source_quad_canonical_sha256);
                result.regenerated_statements+=1;
                result.legacy_timestamp_normalizations.push(normalization);
                continue;
            }
            if projection_statements.iter().any(|row|row.starts_with(&prefix)) {
                return Err("graph.restoreArchive: legacy original metadata conflicts with current native projection".into());
            }
            let mut disposition=disposition.clone();
            disposition["sourceQuadCanonicalSha256"]=json!(source_quad_canonical_sha256);
            result.retained_derived_assertions.push(disposition);
            continue;
        }
        let captured_original_subject = captured_original_subjects.contains(&quad.subject.to_string());
        if let Some(mut disposition) = (legacy_timestamps && !captured_original_subject).then(|| legacy_absent_original_reference(&quad, &projection_statements, &scalar_authorities)).flatten() {
            disposition["sourceQuadCanonicalSha256"] = json!(source_quad_canonical_sha256);
            result.retained_derived_assertions.push(disposition);
            continue;
        }
        let known_projection_vocabulary = [crate::runtime_config::DCTERMS_NS, crate::runtime_config::MDOC_NS,
            crate::runtime_config::WIRE_NS, crate::runtime_config::MNEMO_NS, crate::runtime_config::NFO_NS,
            crate::runtime_config::NIE_NS, "http://www.w3.org/1999/02/22-rdf-syntax-ns#"]
            .iter().any(|prefix| quad.predicate.as_str().starts_with(*prefix));
        if projection_subjects.contains(&quad.subject.to_string()) && known_projection_vocabulary
            && !(legacy_timestamps && legacy_authored_workflow_type(&quad)) {
            let prefix=format!("{} {} ",quad.subject,quad.predicate);
            let native:Vec<_>=projection_statements.iter().filter(|row|row.starts_with(&prefix)).collect();
            // The native projection owns this field and the source may disagree. Strictly
            // terminal; conceded, the source's version is retained as evidence ALONGSIDE the
            // native statements rather than promoted over them, so the projection stays
            // authoritative for the live graph and a reader can see both.
            if concessions.allows(parity::SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY) {
                if let Some(graph) = parity_graph.as_ref() {
                    let source_quad = format!("{statement} .\n");
                    result.retained_derived_assertions.push(parity::disposition(
                        parity::SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY,
                        "source-assertion-outside-projection-authority-v1",
                        quad.subject.to_string().trim_start_matches('<').trim_end_matches('>'),
                        quad.predicate.as_str(), &source_quad, &source_quad_canonical_sha256,
                        &source, evidence_archive_sha256, graph,
                        json!(native.iter().map(|row|row.to_string()).collect::<Vec<_>>())));
                    quad.graph_name = NamedNode::new(graph.as_str()).map_err(|e|e.to_string())?.into();
                    result.user_rdf.push_str(&format!("{quad} .\n"));
                    continue;
                }
            }
            if projection_conflicts.len() < 16 {
                projection_conflicts.entry(quad.predicate.to_string()).or_insert_with(||format!("{statement}; native={native:?}"));
            }
            continue;
        }
        quad.graph_name = user_graph.clone().into();
        result.user_rdf.push_str(&format!("{quad} .\n"));
        result.authored_statements += 1;
    }
    if !projection_conflicts.is_empty() {
        return Err(format!("graph.restoreArchive: source assertions conflict with or are not covered by native projection authority (first sixteen predicates): {}",projection_conflicts.into_values().collect::<Vec<_>>().join(" | ")));
    }
    result.legacy_timestamp_normalizations.sort_by_key(|row| row.to_string());
    result.legacy_timestamp_normalizations.dedup();
    result.retained_derived_assertions.sort_by_key(|row| row.to_string());
    result.retained_derived_assertions.dedup();
    crate::rdf_query_service::validate_rdf_dataset_graph_targets(target, &result.user_rdf, "application/n-quads", None)?;
    Ok(result)
}

fn legacy_saved_wire_endpoint(quad: &oxigraph::model::Quad, snapshot: &Value,
    projected: &std::collections::HashSet<String>, source_user: &str, source_graph: &str, target_graph: &str)
    -> Option<Value> {
    use oxigraph::model::{NamedNode,NamedOrBlankNode,Term};
    let NamedOrBlankNode::NamedNode(subject) = &quad.subject else { return None; };
    let Term::NamedNode(source_object) = &quad.object else { return None; };
    let id = subject.as_str().strip_prefix(&format!("{}:wire:",crate::rdf::graph_subject(target_graph)))?;
    let name = quad.predicate.as_str().strip_prefix(crate::runtime_config::WIRE_NS)?;
    let (doc_field,block_field,is_target) = match name {
        "sourceDocument" => ("sourceDocumentId",None,false),
        "targetDocument" => ("targetDocumentId",None,true),
        "sourceBlock" => ("sourceDocumentId",Some("sourceBlockId"),false),
        "targetBlock" => ("targetDocumentId",Some("targetBlockId"),true),
        _ => return None,
    };
    let wires = snapshot["wires"].as_array()?;
    let mut matching = wires.iter().filter(|wire|wire["id"] == id);
    let wire = matching.next()?; if matching.next().is_some() { return None; }
    let document = wire[doc_field].as_str()?;
    if document.starts_with("urn:") || document.contains(['#','/'])
        || crate::ids::validate_local_id(document,"wire document reference").is_err() { return None; }
    let endpoint_graph = if is_target { wire["targetGraphId"].as_str().filter(|s|!s.is_empty()).unwrap_or(source_graph) } else { source_graph };
    if crate::ids::validate_local_id(endpoint_graph,"wire graph reference").is_err() { return None; }
    let suffix = match block_field {
        None => String::new(),
        Some(field) => {
            let block = wire[field].as_str()?;
            if block.starts_with("urn:") || block.contains(['#','/'])
                || crate::ids::validate_local_id(block,"wire block reference").is_err() { return None; }
            format!("#block-{block}")
        },
    };
    let expected_source = format!("urn:mnemosyne:user:{source_user}:graph:{endpoint_graph}:doc:{document}{suffix}");
    if source_object.as_str() != expected_source { return None; }
    let native = NamedNode::new(format!("{}{suffix}",crate::rdf::document_subject(document))).ok()?;
    let statement = format!("{} {} {}",quad.subject,quad.predicate,native);
    if !projected.contains(&statement) || !projected.contains(&format!("{} <{}> <{}Wire>",
        quad.subject,crate::runtime_config::RDF_TYPE,crate::runtime_config::WIRE_NS)) { return None; }
    Some(json!({"subject":subject.as_str(),"predicate":quad.predicate.as_str(),
        "source":{"iri":source_object.as_str()},"native":{"iri":native.as_str()},
        "rule":"saved-wire-qualified-endpoint-reference-v1","authority":"captured-workspace-yjs",
        "sourceUserId":source_user,"sourceGraphId":source_graph,
        "authoritativeProjectionField":{"root":"wires","entityId":id,"key":doc_field,"value":document},
        "blockField":block_field,"blockValue":block_field.map(|field|wire[field].clone()),
        "endpointGraphId":endpoint_graph,"savedTargetGraphId":wire["targetGraphId"],
        "referenceOnly":true,"endpointExistenceAsserted":false,"fetchAuthority":false,
        "rawSourceRetained":true}))
}

fn legacy_source_evidence_graph(source_user: &str, source_graph: &str, target_graph: &str,
    archive_sha256_value: &str, source_graph_iri: &str) -> String {
    let context=json!([source_user,source_graph,archive_sha256_value,source_graph_iri]);
    format!("{}:user:legacy-evidence:{}",crate::rdf::graph_subject(target_graph),
        archive_sha256(context.to_string().as_bytes()))
}

// Conceded material gets its own NAMESPACE, not merely its own graph: the
// legacy-evidence namespace is verified against the WHOLE STORE as exactly the
// source-only wire set, with an empty-set control, so anything extra under that prefix
// breaks a contract rather than extending one. `user:content-parity:` sits beside it and
// is admissible, since only `:projection:` and the graph root are reserved
// (rdf_authority::is_reserved_rdf_graph_iri). The original note still applies: so the
// source-only wire evidence graph keeps holding exactly the wire anatomy and nothing
// else. That contract is verified quad-for-quad ("complete source-only quad coverage,
// not sampled claims"), and a concession that quietly widened what lives there would
// make a verified invariant merely true-so-far.
fn content_parity_evidence_graph(source_user: &str, source_graph: &str, target_graph: &str,
    archive_sha256_value: &str, source_graph_iri: &str) -> String {
    let context=json!([source_user,source_graph,archive_sha256_value,source_graph_iri,
        crate::crdt_engine::content_parity::RULING]);
    format!("{}:user:content-parity:{}",crate::rdf::graph_subject(target_graph),
        archive_sha256(context.to_string().as_bytes()))
}

// A complete, source-owned RDF wire that is absent from captured current state
// is retained as a legacy assertion, NOT mapped into native wire/document IDs.
// This is deliberately disjoint from authored document/tree recovery.
fn legacy_source_only_wire_subjects(rdf: &str, snapshot: &Value,
    native_subjects: &std::collections::HashSet<String>, source_user: &str,
    source_graph: &str, target_graph: &str) -> Result<std::collections::HashSet<String>,String> {
    use oxigraph::model::{NamedOrBlankNode,Term,GraphName};
    let source=format!("urn:mnemosyne:user:{source_user}:graph:{source_graph}");
    let prefix=format!("{source}:wire:");
    let saved=snapshot["wires"].as_array().ok_or("source wire evidence requires workspace wire inventory")?;
    let saved_ids:std::collections::HashSet<_>=saved.iter().filter_map(|r|r["id"].as_str()).collect();
    let mut groups:std::collections::BTreeMap<String,Vec<oxigraph::model::Quad>>=std::collections::BTreeMap::new();
    for q in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
        let q=q.map_err(|e|e.to_string())?;
        let NamedOrBlankNode::NamedNode(subject)=&q.subject else {continue;};
        let Some(id)=subject.as_str().strip_prefix(&prefix) else {continue;};
        if saved_ids.contains(id) {continue;}
        if !matches!(&q.graph_name,GraphName::NamedNode(n) if n.as_str()==source)
            || crate::ids::validate_local_id(id,"source evidence wire ID").is_err()
            || native_subjects.contains(&format!("<{}:wire:{id}>",crate::rdf::graph_subject(target_graph))) {
            return Err("source-only wire evidence scope or current identity conflict".into());
        }
        groups.entry(subject.as_str().into()).or_default().push(q);
    }
    let fail=||"source-only wire evidence unsupported or incomplete anatomy".to_string();
    for quads in groups.values() {
        let mut fields=std::collections::BTreeMap::new();
        for q in quads {
            if fields.insert(q.predicate.as_str(),&q.object).is_some() {return Err(fail());}
        }
        let type_key=crate::runtime_config::RDF_TYPE;
        let Some(Term::NamedNode(kind))=fields.get(type_key).copied() else {return Err(fail());};
        if kind.as_str()!=format!("{}Wire",crate::runtime_config::WIRE_NS) {return Err(fail());}
        let target_graph_key=format!("{}targetGraph",crate::runtime_config::WIRE_NS);
        let Some(Term::Literal(target_graph))=fields.get(target_graph_key.as_str()).copied() else {return Err(fail());};
        if target_graph.datatype().as_str()!="http://www.w3.org/2001/XMLSchema#string"
            || crate::ids::validate_local_id(target_graph.value(),"source wire target graph").is_err() {return Err(fail());}
        for required in ["sourceDocument","targetDocument","predicate","bidirectional"] {
            if !fields.contains_key(format!("{}{required}",crate::runtime_config::WIRE_NS).as_str()) {return Err(fail());}
        }
        for (predicate,value) in &fields {
            if *predicate==type_key {continue;}
            let key=if *predicate==format!("{}createdAt",crate::runtime_config::MDOC_NS) {"createdAt"}
                else {predicate.strip_prefix(crate::runtime_config::WIRE_NS).ok_or_else(fail)?};
            match key {
                "sourceDocument"|"targetDocument"|"sourceBlock"|"targetBlock"=>{
                    let Term::NamedNode(node)=value else {return Err(fail());};
                    let endpoint_graph=if key.starts_with("target") {target_graph.value()} else {source_graph};
                    let endpoint_prefix=format!("urn:mnemosyne:user:{source_user}:graph:{endpoint_graph}:doc:");
                    let tail=node.as_str().strip_prefix(&endpoint_prefix).ok_or_else(fail)?;
                    let (id,fragment)=tail.split_once('#').map(|(id,f)|(id,Some(f))).unwrap_or((tail,None));
                    if crate::ids::validate_local_id(id,"source wire document reference").is_err()
                        || id.contains(['/',':']) {return Err(fail());}
                    if key.ends_with("Block") {
                        let block=fragment.and_then(|f|f.strip_prefix("block-")).ok_or_else(fail)?;
                        if crate::ids::validate_local_id(block,"source wire block reference").is_err() || block.contains(['/',':','#']) {return Err(fail());}
                        let doc_key=format!("{}{}Document",crate::runtime_config::WIRE_NS,if key.starts_with("target") {"target"} else {"source"});
                        let Some(Term::NamedNode(document))=fields.get(doc_key.as_str()).copied() else {return Err(fail());};
                        if document.as_str()!=format!("{endpoint_prefix}{id}") {return Err(fail());}
                    } else if fragment.is_some() {return Err(fail());}
                },
                "predicate"=>{let Term::NamedNode(node)=value else{return Err(fail());};
                    if node.as_str().starts_with("urn:mnemosyne:user:") || node.as_str().starts_with("urn:mnemosyne:local:") {return Err(fail());}},
                "inverseOf"=>{let Term::NamedNode(node)=value else{return Err(fail());};
                    let id=node.as_str().strip_prefix(&prefix).ok_or_else(fail)?;
                    if crate::ids::validate_local_id(id,"source inverse wire").is_err() {return Err(fail());}},
                "targetGraph"|"sourceSnippet"|"targetSnippet"|"sourceTitle"|"targetTitle"=>{
                    let Term::Literal(lit)=value else {return Err(fail());};
                    if lit.datatype().as_str()!="http://www.w3.org/2001/XMLSchema#string" {return Err(fail());}
                },
                "createdAt"|"snapshotAt"|"deletedAt"=>{
                    let Term::Literal(lit)=value else {return Err(fail());};
                    let numeric=preservation_v2::legacy_decimal_attribute(lit.value()).is_some()
                        && lit.value().parse::<f64>().is_ok_and(|n|n.is_finite()&&n>=0.0);
                    if lit.datatype().as_str()!="http://www.w3.org/2001/XMLSchema#dateTime"
                        || (!numeric&&chrono::DateTime::parse_from_rfc3339(lit.value()).is_err()) {return Err(fail());}
                },
                "bidirectional"=>{let Term::Literal(lit)=value else{return Err(fail());};
                    if lit.datatype().as_str()!="http://www.w3.org/2001/XMLSchema#boolean"
                        || !matches!(lit.value(),"true"|"false"|"True"|"False"|"1"|"0") {return Err(fail());}},
                _=>return Err(fail()),
            }
        }
    }
    let subjects:std::collections::HashSet<_>=groups.into_keys().collect();
    // No half-rewritten incoming relation to a phantom native wire. Connected
    // RDF outside this fully qualified source-only component remains held.
    for q in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
        let q=q.map_err(|e|e.to_string())?;
        if subjects.contains(q.predicate.as_str()) {return Err("source-only wire evidence used as predicate".into());}
        let internal_inverse=q.predicate.as_str()==format!("{}inverseOf",crate::runtime_config::WIRE_NS)
            && matches!(&q.graph_name,GraphName::NamedNode(node) if node.as_str()==source)
            && matches!(&q.subject,NamedOrBlankNode::NamedNode(node) if subjects.contains(node.as_str())
                || node.as_str().strip_prefix(&prefix).is_some_and(|id|saved_ids.contains(id)));
        if matches!(&q.object,Term::NamedNode(node) if subjects.contains(node.as_str())) && !internal_inverse {
            return Err("source-only wire evidence has unsupported incoming relation".into());
        }
    }
    Ok(subjects)
}

fn legacy_projection_normalization(quad: &oxigraph::model::Quad, projected: &std::collections::HashSet<String>) -> Option<Value> {
    use oxigraph::model::{NamedOrBlankNode,Term};
    let NamedOrBlankNode::NamedNode(subject) = &quad.subject else { return None; };
    let Term::Literal(source) = &quad.object else { return None; };
    let predicate = quad.predicate.as_str();
    let is_wire = projected.contains(&format!("{} <{}> <{}Wire>",quad.subject,crate::runtime_config::RDF_TYPE,crate::runtime_config::WIRE_NS));
    let name = if let Some(name)=predicate.strip_prefix(crate::runtime_config::MDOC_NS) {
        if !matches!(name,"createdAt"|"updatedAt"|"lastAccessedAt"|"describedAt"|"order") { return None; }
        name
    } else if is_wire {
        let name=predicate.strip_prefix(crate::runtime_config::WIRE_NS)?;
        if !matches!(name,"deletedAt"|"snapshotAt") { return None; }
        name
    } else { return None; };
    // Only the independently generated workspace value for this exact subject
    // and predicate is eligible. No authored/foreign graph or generic literal
    // value-space matching is introduced.
    let prefix = format!("{} {} ",quad.subject,quad.predicate);
    let values: Vec<_> = projected.iter().filter(|s| s.starts_with(&prefix)).collect();
    if values.len() != 1 { return None; }
    let line = format!("{} .",values[0]);
    let native_quad = RdfParser::from_format(RdfFormat::NTriples).for_slice(line.as_bytes()).next()?.ok()?;
    let Term::Literal(native) = native_quad.object else { return None; };
    let xsd = "http://www.w3.org/2001/XMLSchema#";
    let rule = if name == "order" {
        if source.datatype().as_str() != format!("{xsd}float") || native.datatype().as_str() != format!("{xsd}float") { return None; }
        let a = source.value().parse::<f32>().ok()?;
        let b = native.value().parse::<f32>().ok()?;
        if !a.is_finite() || !b.is_finite() || a != b { return None; }
        "xsd-float-value-space"
    } else {
        let wire_timestamp = is_wire && matches!(name,"createdAt"|"deletedAt"|"snapshotAt")
            && native.datatype().as_str() == format!("{xsd}dateTime");
        if source.datatype().as_str() != format!("{xsd}dateTime")
            || (native.datatype().as_str() != format!("{xsd}string") && !wire_timestamp) { return None; }
        if source.value() == native.value() && (preservation_v2::legacy_decimal_attribute(source.value()).is_some()
            || chrono::DateTime::parse_from_rfc3339(source.value()).is_ok()) {
            "timestamp-datatype-only"
        } else if chrono::DateTime::parse_from_rfc3339(source.value()).ok().zip(chrono::DateTime::parse_from_rfc3339(native.value()).ok())
            .is_some_and(|(a,b)|a.offset().local_minus_utc()==0 && b.offset().local_minus_utc()==0 && a==b) {
            "rfc3339-equal-utc-instant"
        } else {
            // Exact Cloud-1 rdf_materializer.py::_format_datetime_literal
            // numeric path and CPython datetime.fromtimestamp microsecond
            // rounding. This reproduces a source emitter, not an epsilon test.
            let milliseconds = native.value().parse::<f64>().ok()?;
            if !milliseconds.is_finite() || milliseconds < 1e12 { return None; }
            let seconds = milliseconds / 1000.0;
            if seconds >= i64::MAX as f64 { return None; }
            let mut whole = seconds.trunc() as i64;
            let mut micros = (seconds.fract() * 1_000_000.0).round_ties_even() as u32;
            if micros == 1_000_000 { whole = whole.checked_add(1)?; micros = 0; }
            let dt = chrono::DateTime::parse_from_rfc3339(source.value()).ok()?;
            if dt.offset().local_minus_utc() != 0 || dt.timestamp() != whole || dt.timestamp_subsec_nanos() != micros * 1000 { return None; }
            "cloud1-python-epoch-to-rfc3339"
        }
    };
    Some(json!({"subject":subject.as_str(),"predicate":predicate,"rule":rule,
        "source":{"lexical":source.value(),"datatype":source.datatype().as_str()},
        "native":{"lexical":native.value(),"datatype":native.datatype().as_str()}}))
}

fn legacy_authored_workflow_type(quad: &oxigraph::model::Quad) -> bool {
    if quad.predicate.as_str() != crate::runtime_config::RDF_TYPE { return false; }
    let oxigraph::model::Term::NamedNode(kind) = &quad.object else { return false; };
    // These retained workflow-domain assertions are not workspace anatomy.
    // They remain ordinary authored RDF, not a live workflow registration.
    matches!(kind.as_str().strip_prefix("http://mnemosyne.dev/workflow#"),
        Some("AgentNode"|"AgentRun"|"Run"|"Variant"|"Archetype"|"Phase"|"Workflow"|"Contract"|"Adapter"))
}

// Original admission already joined the captured bytes and workspace metadata.
// Qualify only its existing local storage-key representation and exact projection.
fn legacy_owned_artifact_projection(entry: &Value, quad: &oxigraph::model::Quad,
    projected: &std::collections::HashSet<String>) -> bool {
    let Some(user)=entry["sourceUserId"].as_str() else{return false};
    let Some(graph)=entry["sourceGraphId"].as_str() else{return false};
    let Some(id)=entry["original"]["id"].as_str() else{return false};
    let Some(source)=entry["source"]["lexical"].as_str() else{return false};
    let Some(native)=entry["native"]["lexical"].as_str() else{return false};
    let datatype="http://www.w3.org/2001/XMLSchema#string";
    if entry["original"]["ownerKind"]!="artifact" ||
        entry["source"]["datatype"]!=datatype || entry["native"]["datatype"]!=datatype ||
        entry["subject"]!=format!("urn:mnemosyne:local:graph:{graph}:artifact:{id}") ||
        quad.subject.to_string()!=format!("<{}>",entry["subject"].as_str().unwrap_or("")) ||
        quad.predicate.as_str()!="http://mnemosyne.dev/doc#storageKey" ||
        quad.object!=oxigraph::model::Term::Literal(oxigraph::model::Literal::new_simple_literal(source)) ||
        !source.starts_with(&format!("users/{user}/graphs/{graph}/")) ||
        native!=source.replace(&format!("users/{user}/"),"users/default/") || native==source ||
        entry["authoritativeProjectionField"]!=json!({"root":"artifacts","entityId":id,"key":"storageKey","presence":"present","value":source}) {
        return false;
    }
    let prefix=format!("{} {} ",quad.subject,quad.predicate);
    let expected=format!("{prefix}{}",oxigraph::model::Literal::new_simple_literal(native));
    let actual:Vec<_>=projected.iter().filter(|row|row.starts_with(&prefix)).collect();
    actual.len()==1 && *actual[0]==expected
}

// Copied legacy graphs can retain informational original-file RDF without a
// captured original. This is an inert source reference, never a fetch grant or
// a current original-file claim. Saved workspace and native absence both gate it.
fn legacy_absent_original_reference(quad: &oxigraph::model::Quad, projected: &std::collections::HashSet<String>, authorities: &HashMap<String,Value>) -> Option<Value> {
    use oxigraph::model::{NamedOrBlankNode,Term};
    let NamedOrBlankNode::NamedNode(subject) = &quad.subject else { return None; };
    let key = quad.predicate.as_str().strip_prefix(crate::runtime_config::MDOC_NS)?;
    if !matches!(key,"sourceStorageKey"|"sourceOriginalFilename"|"sourceMimeType"|"sourceFileType"|"sourceContentSize") { return None; }
    let field = &authorities.get(subject.as_str())?[key];
    if field["root"] != "documents" || field["key"] != key || field["presence"] != "absent"
        || field["entityId"].as_str().is_none_or(str::is_empty) { return None; }
    let Term::Literal(source) = &quad.object else { return None; };
    if key == "sourceContentSize" {
        if source.datatype().as_str() != "http://www.w3.org/2001/XMLSchema#integer"
            || preservation_v2::legacy_nonnegative_integral_lexical(source.value()).is_none() { return None; }
    } else if source.datatype().as_str() != "http://www.w3.org/2001/XMLSchema#string" || source.value().is_empty() { return None; }
    let prefix = format!("{} {} ",quad.subject,quad.predicate);
    if projected.iter().any(|row| row.starts_with(&prefix)) { return None; }
    Some(json!({"subject":subject.as_str(),"predicate":quad.predicate.as_str(),
        "source":{"lexical":source.value(),"datatype":source.datatype().as_str()},"native":null,
        "authoritativeProjectionField":field,"authority":"captured-workspace-yjs",
        "reason":"original-reference-absent-from-captured-workspace",
        "disposition":"retained-not-rematerialized","rawSourceRetained":true,
        "originalAvailability":"not-established","fetchAuthority":false}))
}

fn legacy_boolean_normalization(quad: &oxigraph::model::Quad, projected: &std::collections::HashSet<String>, authorities: &HashMap<String,Value>) -> Option<Value> {
    use oxigraph::model::{NamedOrBlankNode,Term,Literal,NamedNode};
    let NamedOrBlankNode::NamedNode(subject)=&quad.subject else { return None; };
    let (root,key)=if quad.predicate.as_str()==format!("{}bidirectional",crate::runtime_config::WIRE_NS) { ("wires","bidirectional") }
        else if quad.predicate.as_str()==format!("{}readOnly",crate::runtime_config::MDOC_NS) { ("documents","readOnly") }
        else { return None; };
    let field=&authorities.get(subject.as_str())?[key];
    if field["root"]!=root || field["key"]!=key || field["presence"]!="present" || field["entityId"].as_str().is_none_or(str::is_empty) { return None; }
    let value=field["value"].as_bool()?;
    let Term::Literal(source)=&quad.object else { return None; };
    let datatype="http://www.w3.org/2001/XMLSchema#boolean";
    if source.datatype().as_str()!=datatype || source.value()!=if value {"True"} else {"False"} { return None; }
    let native=Literal::new_typed_literal(value.to_string(),NamedNode::new(datatype).ok()?);
    if !projected.contains(&format!("{} {} {}",quad.subject,quad.predicate,native)) { return None; }
    Some(json!({"subject":subject.as_str(),"predicate":quad.predicate.as_str(),"rule":"cloud1-python-boolean-lexical",
        "source":{"lexical":source.value(),"datatype":datatype},"native":{"lexical":native.value(),"datatype":datatype},
        "authoritativeProjectionField":field}))
}

fn legacy_epoch_decimal_normalization(quad: &oxigraph::model::Quad, projected: &std::collections::HashSet<String>, authorities: &HashMap<String,Value>) -> Option<Value> {
    use oxigraph::model::{NamedOrBlankNode,Term};
    let NamedOrBlankNode::NamedNode(subject)=&quad.subject else { return None; };
    let key=quad.predicate.as_str().strip_prefix(crate::runtime_config::MDOC_NS)?;
    if !matches!(key,"createdAt"|"updatedAt") { return None; }
    let field=&authorities.get(subject.as_str())?[key];
    if field["key"]!=key || field["presence"]!="present" || !matches!(field["root"].as_str(),Some("documents"|"folders"|"artifacts")) { return None; }
    let raw=match &field["value"] {
        Value::Number(number)=>number.to_string(),
        Value::String(text)=>text.clone(),
        _=>return None,
    };
    let expected=preservation_v2::legacy_decimal_attribute(&raw)?;
    let Term::Literal(source)=&quad.object else { return None; };
    if source.datatype().as_str()!="http://www.w3.org/2001/XMLSchema#dateTime"
        || preservation_v2::legacy_decimal_attribute(source.value())?!=expected { return None; }
    let prefix=format!("{} {} ",quad.subject,quad.predicate);
    let rows:Vec<_>=projected.iter().filter(|row|row.starts_with(&prefix)).collect();
    if rows.len()!=1 { return None; }
    let line=format!("{} .",rows[0]);
    let native_quad=RdfParser::from_format(RdfFormat::NTriples).for_slice(line.as_bytes()).next()?.ok()?;
    let Term::Literal(native)=native_quad.object else { return None; };
    if native.datatype().as_str()!="http://www.w3.org/2001/XMLSchema#string"
        || preservation_v2::legacy_decimal_attribute(native.value())?!=expected || native.value()==source.value() { return None; }
    Some(json!({"subject":subject.as_str(),"predicate":quad.predicate.as_str(),"rule":"captured-numeric-epoch-decimal-lexical",
        "source":{"lexical":source.value(),"datatype":source.datatype().as_str()},
        "native":{"lexical":native.value(),"datatype":native.datatype().as_str()},"authoritativeProjectionField":field}))
}

// Only the known document title projection can defer to the captured workspace.
// Empty source and saved titles remain exact evidence even when the existing
// native snapshot uses its named Untitled fallback. No source title is erased.
fn legacy_saved_title_disposition(quad: &oxigraph::model::Quad, projected: &std::collections::HashSet<String>, authorities: &HashMap<String,Value>) -> Option<Value> {
    use oxigraph::model::{NamedOrBlankNode,Term,Literal,NamedNode};
    let NamedOrBlankNode::NamedNode(subject)=&quad.subject else { return None; };
    if quad.predicate.as_str()!=format!("{}title",crate::runtime_config::DCTERMS_NS) { return None; }
    let Term::Literal(source)=&quad.object else { return None; };
    let datatype="http://www.w3.org/2001/XMLSchema#string";
    if source.datatype().as_str()!=datatype { return None; }
    let field=&authorities.get(subject.as_str())?["title"];
    if field["root"]!="documents" || field["key"]!="title" || field["presence"]!="present"
        || field["entityId"].as_str().is_none_or(str::is_empty) { return None; }
    let raw=field["value"].as_str()?;
    let expected=if raw.is_empty() { "Untitled" } else { raw };
    let native=Literal::new_typed_literal(expected,NamedNode::new(datatype).ok()?);
    let prefix=format!("{} {} ",quad.subject,quad.predicate);
    if projected.iter().filter(|row|row.starts_with(&prefix)).count()!=1
        || !projected.contains(&format!("{prefix}{native}")) || source==&native { return None; }
    let reason=if raw.is_empty() { "saved-empty-document-title-native-fallback-v1" }
        else { "saved-document-title-projection-authority-v1" };
    Some(json!({"subject":subject.as_str(),"predicate":quad.predicate.as_str(),
        "source":{"lexical":source.value(),"datatype":datatype},
        "native":{"lexical":native.value(),"datatype":datatype},
        "authoritativeProjectionField":field,"authority":"captured-workspace-yjs",
        "reason":reason,"disposition":"retained-not-rematerialized","rawSourceRetained":true}))
}

fn legacy_saved_scalar_disposition(quad: &oxigraph::model::Quad, projected: &std::collections::HashSet<String>, authorities: &HashMap<String,Value>) -> Option<Value> {
    use oxigraph::model::{NamedOrBlankNode,Term};
    let NamedOrBlankNode::NamedNode(subject)=&quad.subject else { return None; };
    let predicate=quad.predicate.as_str();
    let (key,wire)=if predicate==format!("{}updatedAt",crate::runtime_config::MDOC_NS) { ("updatedAt",false) }
        else if predicate==format!("{}lastAccessedAt",crate::runtime_config::MDOC_NS) { ("lastAccessedAt",false) }
        else if predicate==format!("{}createdAt",crate::runtime_config::MDOC_NS) { ("createdAt",true) }
        else { (predicate.strip_prefix(crate::runtime_config::WIRE_NS)?,true) };
    if wire && !matches!(key,"createdAt"|"deletedAt"|"snapshotAt"|"sourceSnippet"|"targetSnippet"|"inverseOf") { return None; }
    let field=&authorities.get(subject.as_str())?[key];
    if field["key"]!=key || field["entityId"].as_str().is_none_or(str::is_empty)
        || (wire && field["root"]!="wires")
        || (!wire && !matches!(field["root"].as_str(),Some("documents"|"folders"|"artifacts"))) { return None; }
    if key=="lastAccessedAt" && field["root"]!="documents" { return None; }
    let source=if key=="inverseOf" {
        let Term::NamedNode(target)=&quad.object else { return None; };
        // Never admit a foreign/unresolved link as a legacy layout exception.
        if authorities.get(target.as_str())?["inverseOf"]["root"]!="wires" { return None; }
        json!({"iri":target.as_str()})
    } else if matches!(key,"sourceSnippet"|"targetSnippet") {
        // Cloud-1 wire snapshots are cached previews, not a new authored field.
        // Only absence in an existing saved wire can retain a legacy-only value.
        let Term::Literal(value)=&quad.object else { return None; };
        if value.datatype().as_str()!="http://www.w3.org/2001/XMLSchema#string" { return None; }
        json!({"lexical":value.value(),"datatype":value.datatype().as_str()})
    } else {
        let Term::Literal(value)=&quad.object else { return None; };
        if value.datatype().as_str()!="http://www.w3.org/2001/XMLSchema#dateTime" { return None; }
        let numeric=preservation_v2::legacy_decimal_attribute(value.value()).is_some()
            && value.value().parse::<f64>().is_ok_and(|v|v.is_finite()&&v>=0.0);
        if !numeric && chrono::DateTime::parse_from_rfc3339(value.value()).is_err() { return None; }
        json!({"lexical":value.value(),"datatype":value.datatype().as_str()})
    };
    let prefix=format!("{} {} ",quad.subject,quad.predicate);
    let native_rows:Vec<_>=projected.iter().filter(|row|row.starts_with(&prefix)).collect();
    let native=match field["presence"].as_str()? {
        "null" if !wire && key=="updatedAt" && field["root"]=="documents" => {
            if !native_rows.is_empty() || !field["value"].is_null() { return None; }
            Value::Null
        },
        "absent" if wire && matches!(key,"snapshotAt"|"sourceSnippet"|"targetSnippet") => {
            if !native_rows.is_empty() || !field["value"].is_null() { return None; }
            Value::Null
        },
        "absent"|"null" if wire && matches!(key,"createdAt"|"deletedAt"|"inverseOf") => {
            if !native_rows.is_empty() || !field["value"].is_null() { return None; }
            Value::Null
        },
        "present" if matches!(key,"updatedAt"|"snapshotAt"|"lastAccessedAt") => {
            if native_rows.len()!=1 || legacy_projection_normalization(quad,projected).is_some() { return None; }
            let line=format!("{} .",native_rows[0]);
            let native_quad=RdfParser::from_format(RdfFormat::NTriples).for_slice(line.as_bytes()).next()?.ok()?;
            let Term::Literal(value)=native_quad.object else { return None; };
            let raw=crate::json_utils::json_scalar_lexical(Some(&field["value"]))?;
            if field["value"].is_string() {
                if raw!=value.value() || chrono::DateTime::parse_from_rfc3339(&raw).is_err() { return None; }
            } else if field["value"].is_number() {
                let expected=preservation_v2::legacy_decimal_attribute(&raw)?;
                let actual=preservation_v2::legacy_decimal_attribute(value.value())?;
                if expected!=actual { return None; }
            } else { return None; }
            let native=json!({"lexical":value.value(),"datatype":value.datatype().as_str()});
            if native==source { return None; }
            native
        },
        _=>return None,
    };
    Some(json!({"subject":subject.as_str(),"predicate":predicate,"source":source,"native":native,
        "authoritativeProjectionField":field,"authority":"captured-workspace-yjs",
        "reason":"known-derived-assertion-not-current-saved-projection",
        "disposition":"retained-not-rematerialized","rawSourceRetained":true}))
}

fn legacy_order_disposition(quad: &oxigraph::model::Quad, projected: &std::collections::HashSet<String>, authorities: &HashMap<String,Value>) -> Option<Value> {
    use oxigraph::model::{NamedOrBlankNode,Term,Literal,NamedNode};
    let NamedOrBlankNode::NamedNode(subject) = &quad.subject else { return None; };
    if quad.predicate.as_str() != format!("{}order",crate::runtime_config::MDOC_NS) { return None; }
    let Term::Literal(source) = &quad.object else { return None; };
    let datatype = "http://www.w3.org/2001/XMLSchema#float";
    if source.datatype().as_str() != datatype { return None; }
    let source_value = source.value().parse::<f32>().ok()?;
    if !source_value.is_finite() { return None; }
    let field = authorities.get(subject.as_str())?;
    if field["key"] != "order" || !matches!(field["root"].as_str(),Some("documents"|"folders"|"artifacts"))
        || field["entityId"].as_str().is_none_or(str::is_empty) { return None; }
    let value = field["value"].as_f64()?;
    if !value.is_finite() || !(value as f32).is_finite() || source_value == value as f32 { return None; }
    let native = Literal::new_typed_literal(value.to_string(),NamedNode::new(datatype).ok()?);
    if !projected.contains(&format!("{} {} {}",quad.subject,quad.predicate,native)) { return None; }
    Some(json!({"subject":subject.as_str(),"predicate":quad.predicate.as_str(),
        "source":{"lexical":source.value(),"datatype":datatype},
        "native":{"lexical":native.value(),"datatype":datatype},
        "authoritativeProjectionField":field,"authority":"captured-workspace-yjs",
        "disposition":"retained-not-rematerialized","rawSourceRetained":true}))
}

// Cloud-1 access bookkeeping is informational, not content/layout authority.
// A missing captured Y.Map field is NOT filled from RDF and is NOT evidence of
// deletion. Preserve only this named legacy assertion in the disposition ledger.
fn legacy_absent_access_disposition(quad: &oxigraph::model::Quad, projected: &std::collections::HashSet<String>, authorities: &HashMap<String,Value>) -> Option<Value> {
    use oxigraph::model::{NamedOrBlankNode,Term};
    let NamedOrBlankNode::NamedNode(subject) = &quad.subject else { return None; };
    if quad.predicate.as_str() != format!("{}lastAccessedAt",crate::runtime_config::MDOC_NS) { return None; }
    let Term::Literal(source) = &quad.object else { return None; };
    if source.datatype().as_str() != "http://www.w3.org/2001/XMLSchema#dateTime" { return None; }
    let decimal = preservation_v2::legacy_decimal_attribute(source.value()).is_some()
        && source.value().parse::<f64>().is_ok_and(|v| v.is_finite() && v >= 0.0);
    if !decimal && chrono::DateTime::parse_from_rfc3339(source.value()).is_err() { return None; }
    let field = authorities.get(subject.as_str())?;
    if field["key"] != "lastAccessedAt" || field["presence"] != "absent"
        || !matches!(field["root"].as_str(),Some("documents"|"folders"|"artifacts"))
        || field["entityId"].as_str().is_none_or(str::is_empty) { return None; }
    let prefix = format!("{} {} ",quad.subject,quad.predicate);
    if projected.iter().any(|statement| statement.starts_with(&prefix)) { return None; }
    Some(json!({"subject":subject.as_str(),"predicate":quad.predicate.as_str(),
        "source":{"lexical":source.value(),"datatype":source.datatype().as_str()},
        "native":null,"authoritativeProjectionField":field,"authority":"captured-workspace-yjs",
        "reason":"informational-field-absent-from-captured-workspace",
        "disposition":"retained-not-rematerialized","rawSourceRetained":true}))
}

fn count_archive_nquads(rdf_n_quads: &str) -> Result<usize, String> {
    if rdf_n_quads.trim().is_empty() {
        return Ok(0);
    }
    RdfParser::from_format(RdfFormat::NQuads)
        .for_slice(rdf_n_quads.as_bytes())
        .try_fold(0usize, |count, quad| {
            quad.map(|_| count + 1)
                .map_err(|error| format!("parse archive N-Quads for count testimony: {error}"))
        })
}

fn cell_restore_marker_path(graph_dir: &Path) -> PathBuf {
    graph_dir
        .join(CELL_RESTORE_MARKER_DIR)
        .join(CELL_RESTORE_MARKER_FILE)
}

fn directory_has_visible_entries(path: &Path) -> Result<bool, String> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {} entry: {error}", path.display()))?;
        let _ = entry;
        return Ok(true);
    }
    Ok(false)
}

fn require_empty_cell_restore_target(
    app: &AppHandle,
    graph_dir: &Path,
    graph_id: &str,
) -> Result<(), String> {
    let documents = crate::document_service::list_documents(app.clone(), graph_id.to_string())?;
    if !documents.is_empty() {
        return Err(format!(
            "graph.restoreArchive: target graph {graph_id} is not empty ({} documents)",
            documents.len()
        ));
    }
    for path in [
        crate::paths::workspace_ydoc_state_path(graph_dir),
        crate::paths::workspace_snapshot_path(graph_dir),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(_) => return Err(format!(
                "graph.restoreArchive: target graph {graph_id} already has workspace state"
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("inspect {}: {error}", path.display())),
        }
    }
    for path in [
        crate::paths::artifacts_dir(graph_dir),
        crate::paths::images_dir(graph_dir),
        crate::paths::documents_dir(graph_dir),
        graph_dir.join("ydocs/documents"),
        crate::document_tombstone_store::document_tombstones_dir(graph_dir),
        // Orphan text-operation receipts still carry prior write authority.
        // Their contents need not parse or match this restore to occupy it.
        graph_dir.join("artifact-text-operations"),
    ] {
        if directory_has_visible_entries(&path)? {
            return Err(format!(
                "graph.restoreArchive: target graph {graph_id} already has stored content"
            ));
        }
    }
    // Inspect all existing graph names without reseeding them. Only the graph
    // record and its seed bookkeeping can predate an otherwise empty cell.
    if graph_dir.join("store.oxigraph").exists() {
        let store = crate::rdf_store_service::open_graph_store(graph_dir)?;
        let permitted = [crate::rdf_authority::graph_projection_graph_iri(graph_id), crate::rdf_authority::seed_marker_graph_iri(graph_id)];
        for quad in store.iter() {
            let quad = quad.map_err(|error| error.to_string())?;
            if !matches!(&quad.graph_name, oxigraph::model::GraphName::NamedNode(name) if permitted.iter().any(|iri| iri == name.as_str())) {
                return Err(format!("graph.restoreArchive: target graph {graph_id} already has RDF outside empty-cell bookkeeping"));
            }
        }
    }
    Ok(())
}

fn create_or_read_restore_marker(
    graph_dir: &Path,
    marker: &CellArchiveRestoreMarker,
) -> Result<bool, String> {
    let marker_path = cell_restore_marker_path(graph_dir);
    let marker_dir = marker_path
        .parent()
        .ok_or_else(|| "graph.restoreArchive: marker path has no parent".to_string())?;
    crate::storage::create_dir_all(marker_dir)
        .map_err(|error| format!("{CELL_RESTORE_MARKER_STORAGE_ERROR} {error}"))?;
    let mut bytes = serde_json::to_vec(marker)
        .map_err(|error| format!("serialize graph.restoreArchive marker: {error}"))?;
    bytes.push(b'\n');
    let temporary_path = crate::storage_atomic::atomic_temp_path(&marker_path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut temporary = options.open(&temporary_path).map_err(|error| {
        format!(
            "{CELL_RESTORE_MARKER_STORAGE_ERROR} create {}: {error}",
            temporary_path.display()
        )
    })?;
    if let Err(error) = temporary
        .write_all(&bytes)
        .and_then(|_| temporary.sync_all())
    {
        drop(temporary);
        let _ = fs::remove_file(&temporary_path);
        return Err(format!(
            "{CELL_RESTORE_MARKER_STORAGE_ERROR} write {}: {error}",
            temporary_path.display()
        ));
    }
    drop(temporary);

    match fs::hard_link(&temporary_path, &marker_path) {
        Ok(()) => {
            let _ = fs::remove_file(&temporary_path);
            crate::storage_atomic::sync_parent_dir(marker_dir)
                .map_err(|error| format!("{CELL_RESTORE_MARKER_STORAGE_ERROR} {error}"))?;
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temporary_path);
            let existing: CellArchiveRestoreMarker = crate::storage::read_json(&marker_path)
                .map_err(crate::app_error::AppError::message)?;
            if &existing == marker {
                Ok(true)
            } else {
                Err(format!(
                    "graph.restoreArchive: target graph is already claimed by a different restore operation"
                ))
            }
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary_path);
            Err(format!(
                "{CELL_RESTORE_MARKER_STORAGE_ERROR} link {}: {error}",
                marker_path.display()
            ))
        }
    }
}

fn validate_cell_restore_contract(
    app: &AppHandle,
    operation: &CrdtOperation,
    payload: &JsonMap<String, Value>,
    graph: &crate::graph_record_store::GraphRecord,
    graph_dir: &Path,
    parsed: &ParsedGraphArchive,
    bytes: &[u8],
) -> Result<CellArchiveRestoreContract, String> {
    let graph_id = operation.graph_id.trim();
    let source_graph_id = required_payload_string(
        payload,
        &["sourceGraphId", "source_graph_id"],
        "sourceGraphId",
    )?;
    let source_user_id =
        required_payload_string(payload, &["sourceUserId", "source_user_id"], "sourceUserId")?;
    let expected_archive_sha256 = required_payload_string(
        payload,
        &["archiveSha256", "archive_sha256"],
        "archiveSha256",
    )?;
    let plan_digest =
        required_payload_string(payload, &["planDigest", "plan_digest"], "planDigest")?;
    require_lower_sha256(&expected_archive_sha256, "archiveSha256")?;
    require_lower_sha256(&plan_digest, "planDigest")?;
    let target_generation = required_payload_u64(
        payload,
        &["targetGeneration", "target_generation"],
        "targetGeneration",
    )?;
    let expected_document_count = required_payload_usize(
        payload,
        &["expectedDocumentCount", "expected_document_count"],
        "expectedDocumentCount",
    )?;
    let expected_rdf_triple_count = required_payload_usize(
        payload,
        &["expectedRdfTripleCount", "expected_rdf_triple_count"],
        "expectedRdfTripleCount",
    )?;
    if pick(payload, &["includesArtifacts", "includes_artifacts"]).and_then(Value::as_bool)
        != Some(false)
    {
        return Err(
            "graph.restoreArchive: includesArtifacts must be explicitly false for this bounded restore"
                .to_string(),
        );
    }
    if parsed.manifest.includes_artifacts {
        return Err(
            "graph.restoreArchive: archives containing artifacts are outside this bounded restore"
                .to_string(),
        );
    }
    if source_graph_id != graph_id || parsed.manifest.source_graph_id != graph_id {
        return Err("graph.restoreArchive: source graph does not match target graph".to_string());
    }
    if parsed.manifest.source_user_id != source_user_id {
        return Err(
            "graph.restoreArchive: source subject does not match the archive manifest".to_string(),
        );
    }
    let boundary = app
        .try_state::<std::sync::Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .ok_or_else(|| {
            "graph.restoreArchive: a managed single-graph boundary is required".to_string()
        })?;
    if boundary.owner_graph_id() != Some(graph_id)
        || boundary.graph_generation() != Some(target_generation)
        || boundary
            .owner_principal()
            .and_then(|principal| principal.strip_prefix("user:"))
            != Some(source_user_id.as_str())
    {
        return Err(
            "graph.restoreArchive: archive identity does not match the bound cell generation"
                .to_string(),
        );
    }
    let actual_archive_sha256 = archive_sha256(bytes);
    if actual_archive_sha256 != expected_archive_sha256 {
        return Err("graph.restoreArchive: archive digest mismatch".to_string());
    }
    if parsed.documents.len() != expected_document_count {
        return Err(format!(
            "graph.restoreArchive: archive document count {} does not match expected {expected_document_count}",
            parsed.documents.len()
        ));
    }
    let actual_rdf_triple_count = count_archive_nquads(&parsed.rdf_n_quads)?;
    if actual_rdf_triple_count != expected_rdf_triple_count {
        return Err(format!(
            "graph.restoreArchive: archive RDF triple count {actual_rdf_triple_count} does not match expected {expected_rdf_triple_count}"
        ));
    }
    let target_graph_incarnation = graph.incarnation_id.clone().ok_or_else(|| {
        "graph.restoreArchive: target graph is missing an incarnation fence".to_string()
    })?;
    let mut marker = CellArchiveRestoreMarker {
        schema_version: 2,
        rdf_policy: CELL_RESTORE_RDF_POLICY.to_string(),
        operation_id: operation.operation_id.clone(),
        target_graph_id: graph_id.to_string(),
        target_graph_incarnation,
        target_generation,
        archive_sha256: expected_archive_sha256,
        source_graph_id,
        source_user_id,
        plan_digest,
        expected_document_count,
        expected_rdf_triple_count,
        started_at: operation.enqueue_timestamp.clone(),
    };
    let marker_path = cell_restore_marker_path(graph_dir);
    let marker_preexisted = marker_path.is_file();
    if marker_preexisted {
        let existing: CellArchiveRestoreMarker = crate::storage::read_json(&marker_path)
            .map_err(crate::app_error::AppError::message)?;
        marker.started_at = existing.started_at;
    }
    if !marker_preexisted {
        require_empty_cell_restore_target(app, graph_dir, graph_id)?;
    }
    let read_existing = create_or_read_restore_marker(graph_dir, &marker)?;
    Ok(CellArchiveRestoreContract {
        marker,
        marker_path,
        marker_preexisted: marker_preexisted || read_existing,
    })
}

fn require_restore_marker_unchanged(contract: &CellArchiveRestoreContract) -> Result<(), String> {
    let current: CellArchiveRestoreMarker = crate::storage::read_json(&contract.marker_path)
        .map_err(crate::app_error::AppError::message)?;
    if current == contract.marker {
        Ok(())
    } else {
        Err("graph.restoreArchive: durable restore marker changed during execution".to_string())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point — port of importGraphArchive
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) async fn apply(app: &AppHandle, operation: &CrdtOperation) -> Result<Value, String> {
    apply_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

fn archive_failure(error: impl Into<String>, durable_started: bool) -> ApplyOperationError {
    if durable_started {
        ApplyOperationError::retryable_after_hot_commit(error)
    } else {
        ApplyOperationError::terminal(error)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveFailurePoint {
    BeforeDocument,
}

#[cfg(test)]
static FAIL_NEXT_ARCHIVE_STEP: std::sync::OnceLock<
    std::sync::Mutex<Option<(String, ArchiveFailurePoint)>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_archive_step_for_test(
    operation_id: impl Into<String>,
    point: ArchiveFailurePoint,
) {
    *FAIL_NEXT_ARCHIVE_STEP
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((operation_id.into(), point));
}

#[cfg(test)]
fn maybe_fail_archive_step_for_test(
    operation_id: &str,
    point: ArchiveFailurePoint,
) -> Result<(), ApplyOperationError> {
    let mut pending = FAIL_NEXT_ARCHIVE_STEP
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending
        .as_ref()
        .is_some_and(|(expected_id, expected_point)| {
            expected_id == operation_id && *expected_point == point
        })
    {
        pending.take();
        return Err(ApplyOperationError::retryable_after_hot_commit(format!(
            "injected graph.importArchive failure at {point:?} for {operation_id}"
        )));
    }
    Ok(())
}

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    // Defer periodic durable flushes for the duration of this import, and fire
    // one forced flush when it finishes (see cell_durability::ImportGuard).
    let _import_guard = crate::cell_durability::import_guard();
    let payload = obj(&operation.payload);
    let operation_id = operation.operation_id.as_str();
    let operation_kind = operation.kind.trim();
    let restore_existing = match operation_kind {
        "graph.importArchive" => false,
        "graph.restoreArchive" => true,
        other => {
            return Err(ApplyOperationError::terminal(format!(
                "archive handler does not support operation kind {other}"
            )))
        }
    };

    let new_graph_id = nullable_string_value(pick(&payload, &["newGraphId", "new_graph_id"]))
        .ok_or_else(|| format!("{operation_kind}: newGraphId is required"))?;
    if new_graph_id != operation.graph_id {
        return Err(ApplyOperationError::terminal(format!(
            "{operation_kind}: newGraphId must match the operation graph"
        )));
    }
    crate::ids::validate_local_id(&new_graph_id, "graph_id")?;
    let tar_gz_base64 = nullable_string_value(pick(
        &payload,
        &["tarGzBase64", "tar_gz_base64", "dataBase64", "data_base64"],
    ));
    let pending_archive_path = nullable_string_value(pick(
        &payload,
        &["pendingArchivePath", "pending_archive_path"],
    ));
    if tar_gz_base64.is_none() && pending_archive_path.is_none() {
        return Err(ApplyOperationError::terminal(format!(
            "{operation_kind}: tarGzBase64 or pendingArchivePath is required"
        )));
    }

    // Tier B replay guard: if the operation completed previously, return the
    let restore_hash = if restore_existing { Some(restore_envelope_hash(app, operation)?) } else { None };

    // cached envelope without re-touching the filesystem. The ledger entry is
    // written only after the full handler succeeds (see end of this function).
    let ledger_hit = crate::operation_completion_ledger::completion_entry_for(app, operation_id)
        .map_err(|error| {
            ApplyOperationError::retryable_after_hot_commit(format!(
                "read {operation_kind} completion ledger: {error}"
            ))
        })?;
    if let Some(hit) = ledger_hit {
        if restore_existing && (hit.payload_hash != restore_hash || hit.graph_id.as_deref() != Some(new_graph_id.as_str())) {
            return Err(ApplyOperationError::terminal("graph.restoreArchive: completed operation envelope mismatch"));
        }
        if hit.kind == operation_kind {
            let mut cached = hit
                .result
                .as_ref()
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            // Best-effort cleanup of any pending archive file before short-circuiting.
            if let Some(pending) = &pending_archive_path {
                cleanup_pending_archive(app, pending, restore_existing.then_some(new_graph_id.as_str()));
            }
            cached.insert("replayed".to_string(), json!(true));
            return Ok(Value::Object(cached));
        }
        return Err(ApplyOperationError::terminal(format!(
            "completion ledger operation {operation_id} belongs to {}, not {operation_kind}",
            hit.kind
        )));
    }

    // Existence + tag check: if the graph already exists, verify that this is
    // a partial-replay (same operationId) rather than a different op colliding
    // on graph_id. Mismatch is a hard error; match means the prior attempt
    // crashed mid-way and we continue from per-step idempotency.
    let existing = crate::graph_service::list_graphs(app.clone())
        .map_err(ApplyOperationError::retryable_after_hot_commit)?
        .into_iter()
        .find(|graph| graph.graph_id == new_graph_id);
    if restore_existing && existing.is_none() {
        return Err(ApplyOperationError::terminal(format!(
            "graph.restoreArchive: target graph {new_graph_id} does not exist"
        )));
    }
    if !restore_existing {
        if let Some(existing) = &existing {
            if existing.created_by_operation_id.as_deref() != Some(operation_id) {
                return Err(ApplyOperationError::terminal(format!(
                    "graph.importArchive: graph {new_graph_id} already exists with a different originating operation"
                )));
            }
        }
        // partial-replay: continue. create_graph below will be skipped.
    }

    let restore_marker_preexisted = restore_existing
        && crate::graph_paths::existing_graph_dir(app, &new_graph_id)
            .ok()
            .map(|graph_dir| cell_restore_marker_path(&graph_dir).is_file())
            .unwrap_or(false);
    let durable_started = if restore_existing {
        restore_marker_preexisted
    } else {
        existing.is_some()
    };
    let read_archive_started = Instant::now();
    let bytes: Vec<u8> = if let Some(pending) = &pending_archive_path {
        let read_result = crate::pending_upload_service::read_pending_upload_file(
            app.clone(),
            crate::pending_upload_service::PendingUploadFileInput {
                graph_id: restore_existing.then(|| new_graph_id.clone()),
                pending_path: pending.clone(),
            },
        );
        let record = read_result.map_err(|error| archive_failure(error, durable_started))?;
        decode_bytes_base64(&record.data_base64).map_err(ApplyOperationError::terminal)?
    } else {
        decode_bytes_base64(tar_gz_base64.as_deref().unwrap_or(""))
            .map_err(ApplyOperationError::terminal)?
    };
    record_import_phase(
        app,
        operation_id,
        "rustImportArchiveReadUploadMs",
        read_archive_started,
    );

    let parse_started = Instant::now();
    if payload.get("formatVersion").and_then(Value::as_u64) == Some(2) {
        if !restore_existing {
            return Err(ApplyOperationError::terminal("preservation v2 requires the owner-bound restore route"));
        }
        return preservation_v2::apply(app, operation, &bytes, pending_archive_path.as_deref(),
            restore_hash.expect("restore envelope checked"));
    }
    let parsed = parse_graph_archive(&bytes).map_err(ApplyOperationError::terminal)?;
    let restore_preflight = if restore_existing {
        Some(preflight_cell_archive(&parsed, &new_graph_id).map_err(ApplyOperationError::terminal)?)
    } else { None };
    // Hold only a registry-owned admission token, never a mutex across await.
    // This refusal precedes a new claim and every workspace/document write.
    let restore_registry = if restore_existing {
        Some(app.try_state::<super::rooms::RoomRegistry>()
            .ok_or_else(|| ApplyOperationError::terminal("graph.restoreArchive: managed room registry required"))?)
    } else { None };
    let _restore_rooms = restore_registry.as_ref()
        .map(|registry| registry.begin_disk_restore(&new_graph_id))
        .transpose().map_err(ApplyOperationError::terminal)?;
    record_import_phase(app, operation_id, "rustImportArchiveParseMs", parse_started);
    let manifest = &parsed.manifest;
    let requested_title = nullable_string_value(pick(&payload, &["newTitle", "new_title"]))
        .or_else(|| manifest.source_graph_title.clone())
        .unwrap_or_else(|| new_graph_id.clone());
    let requested_description = manifest.source_graph_description.clone();

    let graph_started = Instant::now();
    let (graph, restore_contract) = if restore_existing {
        let graph = existing.expect("restore target existence checked");
        let graph_dir = crate::graph_paths::existing_graph_dir(app, &new_graph_id)
            .map_err(|error| archive_failure(error, restore_marker_preexisted))?;
        let contract = validate_cell_restore_contract(
            app, operation, &payload, &graph, &graph_dir, &parsed, &bytes,
        )
        .map_err(|error| {
            if error.starts_with(CELL_RESTORE_MARKER_STORAGE_ERROR) {
                ApplyOperationError::retryable_after_hot_commit(error)
            } else {
                ApplyOperationError::terminal(error)
            }
        })?;
        (graph, Some(contract))
    } else {
        let graph = match existing {
            Some(graph) => graph,
            None => crate::graph_service::create_graph_service_inner(
                app,
                crate::graph_service::CreateGraphInput {
                    title: requested_title.clone(),
                    graph_id: Some(new_graph_id.clone()),
                    description: requested_description.clone(),
                    operation_id: Some(operation_id.to_string()),
                },
            )
            .map_err(|error| ApplyOperationError::retryable_after_hot_commit(error.to_string()))?,
        };
        (graph, None)
    };
    let title = if restore_existing {
        graph.title.clone()
    } else {
        requested_title
    };
    record_import_phase(
        app,
        operation_id,
        "rustImportArchiveGraphRecordMs",
        graph_started,
    );

    // Workspace Y.Doc: apply the archived full update, rewrite identity
    // paths, persist with a materialized snapshot (same shape the desktop
    // saveWorkspace bridge passes).
    let mut workspace_imported = false;
    let mut workspace_documents: HashMap<String, String> = HashMap::new();
    if let Some(workspace_bytes) = &parsed.workspace_bytes {
        let workspace_started = Instant::now();
        let workspace_doc = Doc::new();
        apply_full_update(&workspace_doc, workspace_bytes, "workspace")
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        rewrite_graph_archive_workspace(&workspace_doc, manifest, &new_graph_id);
        let snapshot = super::workspace_ops::materialize_workspace_snapshot_json(
            &new_graph_id,
            &workspace_doc,
        )
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        if let Some(list) = snapshot.get("documents").and_then(Value::as_array) {
            for entry in list {
                if let (Some(id), Some(doc_title)) = (
                    entry.get("id").and_then(Value::as_str),
                    entry.get("title").and_then(Value::as_str),
                ) {
                    workspace_documents.insert(id.to_string(), doc_title.to_string());
                }
            }
        }
        let save_input: crate::document_types::SaveWorkspaceInput = serde_json::from_value(json!({
            "graphId": new_graph_id,
            "ydocUpdateBase64": encode_bytes_base64(&encode_full_state(&workspace_doc)),
            "snapshot": snapshot,
            "traceOperationId": operation_id,
        }))
        .map_err(|error| {
            ApplyOperationError::retryable_after_hot_commit(format!(
                "assemble save_workspace input: {error}"
            ))
        })?;
        crate::document_persistence_service::save_workspace_with_lease(app.clone(), save_input)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        workspace_imported = true;
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveWorkspaceMs",
            workspace_started,
        );
    }

    // Documents: every deterministic document is part of the transaction.
    // Never bless a partial graph as completion-success.
    let mut documents_imported: usize = 0;
    let mut document_ids: Vec<String> = Vec::new();
    let mut warnings = parsed.warnings.clone();
    let documents_started = Instant::now();
    for (document_id, document_bytes) in &parsed.documents {
        #[cfg(test)]
        maybe_fail_archive_step_for_test(operation_id, ArchiveFailurePoint::BeforeDocument)?;
        match import_one_document(
            app,
            &new_graph_id,
            document_id,
            document_bytes,
            workspace_documents.get(document_id).cloned(),
            operation_id,
        ) {
            Ok((saved_document_id, contract_audit)) => {
                documents_imported += 1;
                document_ids.push(saved_document_id);
                warnings.extend(contract_audit);
            }
            Err(error) => {
                return Err(ApplyOperationError::retryable_after_hot_commit(format!(
                    "import document {document_id}: {error}"
                )));
            }
        }
    }
    record_import_phase(
        app,
        operation_id,
        "rustImportArchiveDocumentsMs",
        documents_started,
    );

    // RDF: rewrite source identity → local graph identity, load as n-quads.
    let mut rdf_triple_count: usize = 0;
    if !parsed.rdf_n_quads.trim().is_empty() {
        let rewrite_started = Instant::now();
        let rewritten = if let Some(preflight) = &restore_preflight {
            preflight.user_rdf.clone()
        } else { rewrite_graph_archive_rdf(&parsed.rdf_n_quads, manifest, &new_graph_id) };
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveRewriteRdfMs",
            rewrite_started,
        );
        let load_started = Instant::now();
        let result = crate::rdf_service::load_rdf_dataset(
            app.clone(),
            crate::rdf_service::RdfLoadInput {
                graph_id: new_graph_id.clone(),
                data: rewritten,
                format: "application/n-quads".to_string(),
                base_iri: None,
                target_graph_iri: None,
            },
        )
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        rdf_triple_count = restore_contract
            .as_ref()
            .map(|contract| contract.marker.expected_rdf_triple_count)
            .unwrap_or(result.quad_count);
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveLoadRdfMs",
            load_started,
        );
    }

    let graph_value = serde_json::to_value(&graph).map_err(|error| {
        ApplyOperationError::retryable_after_hot_commit(format!("serialize graph record: {error}"))
    })?;
    let mut envelope = json!({
        "type": "import_graph",
        "graph_id": new_graph_id,
        "graphId": new_graph_id,
        "title": title,
        "document_count": documents_imported,
        "documentCount": documents_imported,
        "document_ids": document_ids,
        "documentIds": document_ids,
        "rdf_triple_count": rdf_triple_count,
        "rdfTripleCount": rdf_triple_count,
        "source_graph_id": manifest.source_graph_id,
        "sourceGraphId": manifest.source_graph_id,
        "source_user_id": manifest.source_user_id,
        "sourceUserId": manifest.source_user_id,
        "archive_size_bytes": bytes.len(),
        "archiveSizeBytes": bytes.len(),
        "includes_artifacts": manifest.includes_artifacts,
        "includesArtifacts": manifest.includes_artifacts,
        "workspace_imported": workspace_imported,
        "workspaceImported": workspace_imported,
        "warnings": warnings,
        "graph": graph_value,
    });
    if let Some(contract) = &restore_contract {
        require_restore_marker_unchanged(contract)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        let object = envelope
            .as_object_mut()
            .expect("archive result envelope is an object");
        object.insert("restored_existing_graph".to_string(), json!(true));
        object.insert("restoredExistingGraph".to_string(), json!(true));
        if let Some(preflight) = &restore_preflight {
            object.insert("rdfPolicy".into(), json!(CELL_RESTORE_RDF_POLICY));
            object.insert("authoredRdfStatementCount".into(), json!(preflight.authored_statements));
            object.insert("regeneratedRdfStatementCount".into(), json!(preflight.regenerated_statements));
            object.insert("userRdfGraphIri".into(), json!(crate::rdf_authority::user_rdf_graph_iri(&new_graph_id)));
        }
        object.insert(
            "archive_sha256".to_string(),
            json!(contract.marker.archive_sha256),
        );
        object.insert(
            "archiveSha256".to_string(),
            json!(contract.marker.archive_sha256),
        );
        object.insert(
            "plan_digest".to_string(),
            json!(contract.marker.plan_digest),
        );
        object.insert("planDigest".to_string(), json!(contract.marker.plan_digest));
        object.insert(
            "target_generation".to_string(),
            json!(contract.marker.target_generation),
        );
        object.insert(
            "targetGeneration".to_string(),
            json!(contract.marker.target_generation),
        );
        object.insert(
            "restore_claim_replayed".to_string(),
            json!(contract.marker_preexisted),
        );
        object.insert(
            "restoreClaimReplayed".to_string(),
            json!(contract.marker_preexisted),
        );
    }

    // Append the Tier B completion entry only after every step has succeeded.
    // Failure leaves the outer journal operation retryable under the same ID.
    let entry = crate::operation_completion_ledger::OperationCompletionEntry {
        schema_version: 1,
        operation_id: operation_id.to_string(),
        kind: operation_kind.to_string(),
        graph_id: Some(new_graph_id.clone()),
        completed_at: operation.enqueue_timestamp.clone(),
        payload_hash: restore_hash,
        result: Some(envelope.clone()),
    };
    let ledger_started = Instant::now();
    crate::operation_completion_ledger::append_completion_entry(app, entry)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    record_import_phase(
        app,
        operation_id,
        "rustImportArchiveCompletionLedgerMs",
        ledger_started,
    );

    if let Some(pending) = &pending_archive_path {
        let cleanup_started = Instant::now();
        cleanup_pending_archive(app, pending, restore_existing.then_some(new_graph_id.as_str()));
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveCleanupPendingMs",
            cleanup_started,
        );
    }

    Ok(envelope)
}

/// Most block-contract audit receipts reported per imported document.
const CONTRACT_AUDIT_LIMIT: usize = 20;

/// The block contract over an archived document, as an audit: an archive
/// carries Y.Doc bytes (CRDT state with its history), not JSON, and a restore
/// must reproduce them exactly (the cell-restore content-parity contract), so
/// import never rewrites them. It reports what the normaliser WOULD rewrite
/// on the document's next JSON write, so the exposure is visible at import.
fn contract_audit(document_id: &str, tiptap_json: &Value) -> Vec<String> {
    let nodes = tiptap_json
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let (_, receipts) = super::block_contract::normalise_doc_content(nodes, "archive-audit");
    let rewrites: Vec<&String> = receipts
        .iter()
        .filter(|w| super::block_contract::is_rewrite_warning(w))
        .collect();
    let mut out: Vec<String> = rewrites
        .iter()
        .take(CONTRACT_AUDIT_LIMIT)
        .map(|w| format!("document {document_id}: stored bytes unchanged; next JSON write would apply: {w}"))
        .collect();
    if rewrites.len() > CONTRACT_AUDIT_LIMIT {
        out.push(format!(
            "document {document_id}: {} more block-contract receipts not listed",
            rewrites.len() - CONTRACT_AUDIT_LIMIT
        ));
    }
    out
}

/// One archived document Y.Doc → create_document + save_document (the same
/// internals the desktop frontend's bridge commands run). Returns the saved
/// record's documentId and the block-contract audit receipts.
fn import_one_document(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    document_bytes: &[u8],
    workspace_title: Option<String>,
    operation_id: &str,
) -> Result<(String, Vec<String>), String> {
    let doc = Doc::new();
    apply_full_update(&doc, document_bytes, "document")?;
    let snapshot = super::projection::materialize_ydoc(&doc, document_id);
    let audit = contract_audit(document_id, &snapshot.tiptap_json);
    let tiptap_xml = super::projection::ydoc_to_tiptap_xml(&doc);
    let document_title = workspace_title.unwrap_or_else(|| document_id.to_string());

    let create_input: crate::document_types::CreateDocumentInput = serde_json::from_value(json!({
        "graphId": graph_id,
        "title": document_title,
        "documentId": document_id,
    }))
    .map_err(|error| format!("assemble create_document input: {error}"))?;
    crate::document_service::create_imported_document_with_lease(app.clone(), create_input)?;

    let candidate = json!({
        "graphId": graph_id,
        "documentId": document_id,
        "title": document_title,
        "body": snapshot.body,
        "tiptapXml": tiptap_xml,
        "tiptapJson": snapshot.tiptap_json,
        "ydocUpdateBase64": encode_bytes_base64(&encode_full_state(&doc)),
        "tree": snapshot.tree_json,
        "blocks": snapshot.blocks_json,
        "traceOperationId": format!("{operation_id}-{document_id}"),
    });
    if super::document_ops::reconcile_matching_document_projection(
        app,
        graph_id,
        document_id,
        &candidate,
    )?
    .is_none()
    {
        let save_input: crate::document_types::SaveDocumentInput =
            serde_json::from_value(candidate)
                .map_err(|error| format!("assemble save_document input: {error}"))?;
        crate::document_persistence_service::save_document_with_lease(app.clone(), save_input)?;
    }
    Ok((document_id.to_string(), audit))
}

/// Best-effort pending-archive cleanup (the TS .catch(console.warn) paths).
fn cleanup_pending_archive(app: &AppHandle, pending_path: &str, graph_id: Option<&str>) {
    if let Err(error) = crate::pending_upload_service::cleanup_pending_upload(
        app.clone(),
        crate::pending_upload_service::PendingUploadFileInput {
            graph_id: graph_id.map(str::to_string),
            pending_path: pending_path.to_string(),
        },
    ) {
        log::warn!("[graph.importArchive] pending graph archive cleanup failed: {error}");
    }
}

#[cfg(test)]
mod cell_restore_contract_tests {
    use super::*;
    use uuid::Uuid;

    fn gzip_for_limit_test(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn cell_restore_compressed_input_size_is_inclusive_before_inflation() {
        // The production caller supplies the full input slice length, including
        // gzip headers. Exercise its admission arithmetic without allocating a
        // half-gigabyte attack fixture or claiming a total-process memory cap.
        assert!(require_compressed_archive_size(0).is_ok());
        assert!(require_compressed_archive_size(MAX_EXTRACTED_SIZE).is_ok());
        assert!(require_compressed_archive_size(MAX_EXTRACTED_SIZE + 1)
            .unwrap_err().contains("including its gzip header"));
    }

    #[test]
    fn cell_restore_gzip_expansion_is_bounded_during_read_including_zero_header_tail() {
        let exact = vec![0u8; 1024];
        assert_eq!(read_gzip_bounded(&gzip_for_limit_test(&exact), 1024).unwrap(), exact);
        // The first zero tar header does not excuse inflating its following tail.
        let expanded_tail = vec![0u8; 4096];
        let compressed = gzip_for_limit_test(&expanded_tail);
        assert!(compressed.len() < 1024);
        assert!(read_gzip_bounded(&compressed, 1024).unwrap_err().contains("exceeds 1024 bytes"));
        assert!(read_gzip_bounded(&gzip_for_limit_test(&[0]), 0).is_err());
        assert!(read_gzip_bounded(&gzip_for_limit_test(&[]), 0).unwrap().is_empty());
        assert!(read_gzip_bounded(b"not gzip", 1024).is_err());
    }

    #[test]
    fn cell_restore_tar_tail_is_zero_and_bounded() {
        assert!(read_tar_entries(&vec![0; MAX_TAR_TRAILING_BYTES]).unwrap().is_empty());
        assert!(read_tar_entries(&vec![0; MAX_TAR_TRAILING_BYTES + TAR_BLOCK_SIZE]).unwrap_err().contains("Invalid tar tail"));
        let mut tail = vec![0; TAR_BLOCK_SIZE * 2];
        tail[TAR_BLOCK_SIZE] = 1;
        assert!(read_tar_entries(&tail).unwrap_err().contains("Invalid tar tail"));
        assert!(read_tar_entries(&[1, 2, 3]).unwrap_err().contains("incomplete nonzero header"));
    }

    fn independent_archive() -> ParsedGraphArchive {
        let fixture: Value = serde_json::from_str(include_str!("../../tests/fixtures/owned-restore-v1.json")).unwrap();
        let bytes = decode_bytes_base64(fixture["archiveBase64"].as_str().unwrap()).unwrap();
        parse_graph_archive(&bytes).unwrap()
    }

    #[test]
    fn cell_restore_preflight_preserves_nonowned_title_and_rejects_unresolved_identity() {
        let mut archive = independent_archive();
        let graph = archive.manifest.source_graph_id.clone();
        let source = format!("urn:mnemosyne:user:{}:graph:{graph}", archive.manifest.source_user_id);
        archive.rdf_n_quads.push_str(&format!("<urn:external:authored-subject> <http://purl.org/dc/terms/title> \"Not a workspace title\"@en <{source}> .\n"));
        let preflight = preflight_cell_archive(&archive, &graph).unwrap();
        assert_eq!(preflight.regenerated_statements, 5);
        assert_eq!(preflight.authored_statements, 5);
        assert!(preflight.user_rdf.contains("\"Not a workspace title\"@en"));
        archive.rdf_n_quads.push_str(&format!("<{source}:doc:missing-physical-document> <urn:custom:p> \"orphan\" <{source}> .\n"));
        assert!(preflight_cell_archive(&archive, &graph).err().unwrap().contains("unresolved source document identity"));
    }

    #[test]
    fn conceded_unresolved_identity_is_retained_as_evidence_and_recorded() {
        use crate::crdt_engine::content_parity as parity;
        let mut archive = independent_archive();
        let graph = archive.manifest.source_graph_id.clone();
        let source = format!("urn:mnemosyne:user:{}:graph:{graph}", archive.manifest.source_user_id);
        archive.rdf_n_quads.push_str(&format!("<{source}:doc:missing-physical-document> <urn:custom:p> \"orphan\" <{source}> .\n"));

        // Strict, and with the concession named but no evidence graph to retain into:
        // both refuse, because a concession that dropped the quad would invert the
        // meaning of content parity.
        let named = parity::Concessions::parse(Some(&json!([parity::UNRESOLVED_SOURCE_DOCUMENT_IDENTITY]))).unwrap();
        for concessions in [&parity::Concessions::none(), &named] {
            let error = preflight_cell_archive_with_content_parity(&archive, &graph, false,
                &HashMap::new(), &std::collections::BTreeSet::new(), None, concessions)
                .err().expect("no evidence graph means the strict error stands");
            assert!(error.contains("unresolved source document identity"), "{error}");
        }

        // Conceded, with an evidence graph: admitted, the quad retained there, and the
        // dangling id recorded as a reference-only disposition.
        let preflight = preflight_cell_archive_with_content_parity(&archive, &graph, false,
            &HashMap::new(), &std::collections::BTreeSet::new(), Some("deadbeef"), &named)
            .expect("conceded unresolved identity is admitted");
        let row = preflight.retained_derived_assertions.iter()
            .find(|row| row["reason"] == "unresolved-source-document-identity-v1")
            .expect("the concession is recorded");
        assert_eq!(row["unresolvedDocumentId"], "missing-physical-document");
        assert_eq!(row["contentParity"]["concession"], parity::UNRESOLVED_SOURCE_DOCUMENT_IDENTITY);
        assert_eq!(row["contentParity"]["ruling"], parity::RULING);
        assert_eq!(row["referenceOnly"], true);
        assert_eq!(row["currentEntityExistenceAsserted"], false);
        assert_eq!(row["rawSourceRetained"], true);
        let evidence = row["evidenceGraph"].as_str().expect("an evidence graph is named");
        assert!(preflight.user_rdf.contains(evidence), "the quad is retained in the evidence graph");
        assert!(preflight.user_rdf.contains("missing-physical-document"),
            "content parity means the assertion survives");

        // Naming only the OTHER concession must not admit this one.
        let other = parity::Concessions::parse(Some(&json!([parity::SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY]))).unwrap();
        assert!(preflight_cell_archive_with_content_parity(&archive, &graph, false,
            &HashMap::new(), &std::collections::BTreeSet::new(), Some("deadbeef"), &other)
            .err().unwrap().contains("unresolved source document identity"));
    }

    #[test]
    fn cell_restore_preflight_decodes_documents_and_workspace_before_any_target_write() {
        let mut archive = independent_archive();
        let graph = archive.manifest.source_graph_id.clone();
        archive.documents[0].1 = vec![255];
        assert!(preflight_cell_archive(&archive, &graph).is_err());
        let mut archive = independent_archive();
        archive.workspace_bytes = Some(vec![255]);
        assert!(preflight_cell_archive(&archive, &graph).is_err());
    }

    fn marker(operation_id: &str) -> CellArchiveRestoreMarker {
        CellArchiveRestoreMarker {
            schema_version: 2,
            rdf_policy: CELL_RESTORE_RDF_POLICY.to_string(),
            operation_id: operation_id.to_string(),
            target_graph_id: "graph-a".to_string(),
            target_graph_incarnation: "incarnation-a".to_string(),
            target_generation: 1,
            archive_sha256: "a".repeat(64),
            source_graph_id: "graph-a".to_string(),
            source_user_id: "test-owner".to_string(),
            plan_digest: "b".repeat(64),
            expected_document_count: 2,
            expected_rdf_triple_count: 2,
            started_at: "1720000000000".to_string(),
        }
    }

    #[test]
    fn restore_marker_is_create_only_exact_replay_and_closed() {
        let graph_dir =
            std::env::temp_dir().join(format!("garden-cell-restore-marker-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&graph_dir).expect("create marker test graph dir");
        let first = marker("restore-op-1");
        assert!(!create_or_read_restore_marker(&graph_dir, &first).expect("create marker"));
        assert!(create_or_read_restore_marker(&graph_dir, &first).expect("replay marker"));

        let mut conflicting = first.clone();
        conflicting.plan_digest = "c".repeat(64);
        assert!(create_or_read_restore_marker(&graph_dir, &conflicting).is_err());

        let mut extended = serde_json::to_value(&first).expect("serialize marker");
        extended["unexpected"] = json!(true);
        assert!(serde_json::from_value::<CellArchiveRestoreMarker>(extended).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cell_restore_marker_path(&graph_dir))
                .expect("marker metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(graph_dir);
    }

    #[test]
    fn restore_count_testimony_uses_a_real_nquads_parser() {
        let quads = concat!(
            "<urn:s:1> <urn:p> <urn:o:1> <urn:g> .\n",
            "<urn:s:2> <urn:p> \"literal\" <urn:g> .\n",
        );
        assert_eq!(count_archive_nquads(quads).expect("valid N-Quads"), 2);
        assert_eq!(count_archive_nquads("\n\t").expect("empty N-Quads"), 0);
        assert!(count_archive_nquads("not n-quads").is_err());
    }

    #[test]
    fn workspace_rewrite_retargets_intra_graph_wires_and_leaves_anomalies() {
        let manifest = GraphArchiveManifest {
            source_user_id: "u1".to_string(),
            source_graph_id: "default".to_string(),
            source_graph_title: None,
            source_graph_description: None,
            includes_artifacts: false,
        };
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            let wires = txn.get_or_insert_map("wires");
            // Intra-graph wire: target is the source graph itself.
            let intra = wires.insert(&mut txn, "w-intra", yrs::MapPrelim::default());
            intra.insert(&mut txn, "targetGraphId", "default".to_string());
            // Cross-graph anomaly: target is some other graph.
            let cross = wires.insert(&mut txn, "w-cross", yrs::MapPrelim::default());
            cross.insert(&mut txn, "targetGraphId", "sophia-labs".to_string());
        }

        rewrite_graph_archive_workspace(&doc, &manifest, "default_8_25_26_3");

        let mut txn = doc.transact_mut();
        let wires = txn.get_or_insert_map("wires");
        let read_target = |txn: &yrs::TransactionMut, wire_id: &str| -> String {
            let Some(Out::YMap(wire)) = wires.get(txn, wire_id) else {
                panic!("missing wire {wire_id}");
            };
            match wire.get(txn, "targetGraphId") {
                Some(Out::Any(Any::String(value))) => value.to_string(),
                other => panic!("targetGraphId not a string: {other:?}"),
            }
        };
        assert_eq!(
            read_target(&txn, "w-intra"),
            "default_8_25_26_3",
            "intra-graph wire must be retargeted to the new graph id"
        );
        assert_eq!(
            read_target(&txn, "w-cross"),
            "sophia-labs",
            "cross-graph anomaly must be left untouched (fails gracefully, not rewritten)"
        );
    }
}
