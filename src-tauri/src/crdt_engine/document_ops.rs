//! Document content operations (document.write, block.*) for headless cells.
//!
//! Port of frontend/src/native/native-local-runtime.ts:
//! - writeDocument (~line 2127) + saveDocumentSnapshot (~1067)
//! - blockToTipTapNode / inlineTipTapContentFromBlock / normalizeBlockMark /
//!   tiptapMarkFromBlockMark (~3862-4012)
//!
//! Writes flow through the room registry so connected y-websocket clients
//! see API writes live, then persist via the same Rust functions the
//! desktop frontend's Tauri commands use (save_document → record + RDF).

use crate::app_runtime::AppHandle;
use crate::crdt_engine::{
    builder,
    executor::{ApplyOperationError, ApplyOperationResult},
    projection,
    rooms::{Room, RoomRegistry},
};
use crate::crdt_queue::CrdtOperation;
use crate::document_paths::document_dir;
use crate::document_record_store::read_document_record;
use crate::document_types::{DocumentRecord, SaveDocumentInput};
use crate::graph_paths::existing_graph_dir;
use crate::ydoc_paths::checked_document_ydoc_state_path;
use base64::Engine;
use serde_json::{json, Map, Value};
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::types::text::YChange;
use yrs::types::xml::{XmlFragment, XmlOut};
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Doc, Map as YMap, Out, ReadTxn, Text, Transact, TransactionMut, Update, WriteTxn,
    XmlElementRef,
};

use super::block_ops::{self, js_finite_number, js_string_value};

fn obj(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn str_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) if !other.is_null() => {
            Some(crate::crdt_engine::projection::js_string_pub(other))
        }
        _ => None,
    }
}

fn num_field(map: &Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key).and_then(Value::as_f64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocumentWriteRevisionDisposition {
    Apply,
    Replay,
}

#[cfg(test)]
static FAIL_NEXT_DOCUMENT_WRITE_AFTER_HOT: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
static FAIL_NEXT_RECREATION_BEFORE_WORKSPACE: std::sync::OnceLock<
    std::sync::Mutex<Option<String>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_document_write_after_hot_for_test(operation_id: impl Into<String>) {
    *FAIL_NEXT_DOCUMENT_WRITE_AFTER_HOT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(operation_id.into());
}

#[cfg(test)]
pub(crate) fn fail_next_document_recreation_before_workspace_for_test(
    operation_id: impl Into<String>,
) {
    *FAIL_NEXT_RECREATION_BEFORE_WORKSPACE
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(operation_id.into());
}

#[cfg(test)]
fn maybe_fail_document_write_after_hot_for_test(
    operation_id: &str,
) -> Result<(), ApplyOperationError> {
    let mut pending = FAIL_NEXT_DOCUMENT_WRITE_AFTER_HOT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending.as_deref() == Some(operation_id) {
        pending.take();
        return Err(ApplyOperationError::retryable_after_hot_commit(format!(
            "injected document.write failure after hot commit for {operation_id}"
        )));
    }
    Ok(())
}

#[cfg(test)]
fn maybe_fail_document_recreation_before_workspace_for_test(
    operation_id: &str,
) -> Result<(), ApplyOperationError> {
    let mut pending = FAIL_NEXT_RECREATION_BEFORE_WORKSPACE
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending.as_deref() == Some(operation_id) {
        pending.take();
        return Err(ApplyOperationError::retryable_after_hot_commit(format!(
            "injected document recreation failure before workspace commit for {operation_id}"
        )));
    }
    Ok(())
}

fn supplied_comments_match(doc: &yrs::Doc, comments: &Map<String, Value>) -> bool {
    if comments.is_empty() {
        return true;
    }
    let txn = doc.transact();
    let Some(existing) = txn.get_map("comments") else {
        return false;
    };
    comments.iter().all(|(comment_id, expected)| {
        matches!(
            existing.get(&txn, comment_id.as_str()),
            Some(Out::Any(ref actual)) if any_json(actual) == *expected
        )
    })
}

/// Validate optimistic concurrency before touching the live Y.Doc. A journal
/// replay is admissible at expected+1 only when both cold and hot semantic
/// state already equal the requested write. Anything else conflicts before a
/// broadcast or update-v1.bin write can occur.
async fn preflight_document_write_revision(
    document_id: &str,
    title: &str,
    desired_tiptap_json: &Value,
    comments: &Map<String, Value>,
    existing_record: Option<&DocumentRecord>,
    room: &Room,
    expected_revision: Option<&Value>,
) -> Result<DocumentWriteRevisionDisposition, String> {
    let Some(expected_value) = expected_revision.filter(|value| !value.is_null()) else {
        return Ok(DocumentWriteRevisionDisposition::Apply);
    };
    let expected = expected_value
        .as_u64()
        .ok_or_else(|| "expectedRevision must be a non-negative integer".to_string())?;
    let current = existing_record.map(|record| record.revision).unwrap_or(0);
    if current == expected {
        return Ok(DocumentWriteRevisionDisposition::Apply);
    }
    if current != expected.saturating_add(1) {
        return Err(format!(
            "revision conflict: expected {expected}, actual {current}"
        ));
    }

    let Some(record) = existing_record else {
        return Err(format!(
            "revision conflict: expected {expected}, actual {current}; replay record is missing"
        ));
    };
    let desired = projection::materialize_tiptap_json(desired_tiptap_json, document_id);
    let cold_matches = record.title == title
        && record.tiptap_json.as_ref() == Some(&desired.tiptap_json)
        && record.body.eq(&desired.body);
    let hot_matches = room
        .with_doc(|doc| {
            let current = projection::materialize_ydoc(doc, document_id);
            current.tiptap_json.eq(&desired.tiptap_json)
                && current.body.eq(&desired.body)
                && supplied_comments_match(doc, comments)
        })
        .await;
    if !cold_matches || !hot_matches {
        return Err(format!(
            "revision conflict: expected {expected}, actual {current}; replay content differs"
        ));
    }
    Ok(DocumentWriteRevisionDisposition::Replay)
}

pub(crate) async fn document_write(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> Result<Value, String> {
    document_write_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

pub(crate) async fn document_write_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let payload = obj(&operation.payload);
    let graph_id = operation.graph_id.clone();
    let document_id = operation
        .document_id
        .clone()
        .or_else(|| str_field(&payload, "documentId"))
        .ok_or_else(|| "write document operation is missing documentId".to_string())?;

    let parent_id = str_field(&payload, "parentId");
    let order = num_field(&payload, "order")
        .or_else(|| operation.enqueue_timestamp.parse::<f64>().ok())
        .unwrap_or(0.0);

    // Content derivation order mirrors writeDocument: tiptapJson payload,
    // then parsed `content` string, then blocks array.
    let mut source_format: Option<String> = None;
    let mut warnings: Vec<String> = Vec::new();
    let mut derived_title: Option<String> = None;
    // True only on the `content`-string path: that parser mints FRESH random
    // block ids (parse_write_content_for_operation → ensure_block_ids),
    // because a plain markdown/text/HTML string carries no ids. When the
    // caller supplies `tiptapJson` or `blocks` they own the ids and we must
    // not touch them.
    let mut from_parsed_content = false;
    let tiptap_json: Value = if let Some(tj) = payload.get("tiptapJson").filter(|v| v.is_object()) {
        tj.clone()
    } else if let Some(content) = payload.get("content").and_then(Value::as_str) {
        let parsed = super::content_parse::parse_write_content_for_operation(
            content,
            payload.get("format").and_then(Value::as_str),
            &operation.operation_id,
        )?;
        source_format = Some(parsed.source_format);
        warnings = parsed.warnings;
        derived_title = parsed.derived_title;
        from_parsed_content = true;
        parsed.tiptap_json
    } else {
        tiptap_json_from_blocks(payload.get("blocks"))
    };

    let graph_dir = existing_graph_dir(app, &graph_id)?;
    // A same-ID write is the only trusted recreation path. Finish any
    // interrupted delete tail while retaining its exact deletion fence; stale
    // document/sidecar bytes must never be hydrated into the new authority.
    let recreation = crate::document_delete_service::prepare_tombstoned_document_recreation(
        app,
        &graph_id,
        &document_id,
    )
    .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    let manifest = document_dir(&graph_dir, &document_id)?.join("document.json");
    let existing_record = if manifest.is_file() {
        Some(read_document_record(&graph_dir, &manifest)?)
    } else {
        None
    };
    let title = str_field(&payload, "title")
        .or(derived_title)
        .or_else(|| existing_record.as_ref().map(|record| record.title.clone()))
        .unwrap_or_else(|| document_id.clone());
    let title = {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            "Untitled".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let title = if existing_record.is_some() {
        crate::ids::normalize_stored_title(&title)?
    } else {
        crate::ids::normalize_title(&title)?
    };

    // Rewrite the room's fragment in one transaction: live collaborators
    // receive the change as a normal sync update.
    let registry = app.state::<RoomRegistry>();
    let state_path = checked_document_ydoc_state_path(&graph_dir, &document_id)?;
    let room_key = format!("doc:{graph_id}:{document_id}");
    let room = if let Some(tombstone) = recreation.as_ref() {
        registry
            .recreate_tombstoned_document_room(&room_key, state_path, &tombstone.deletion_id)
            .await
    } else {
        registry.get_or_create(&room_key, state_path).await
    }
    .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    let content_nodes: Vec<Value> = tiptap_json
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let comments = payload
        .get("comments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let desired_tiptap_json = json!({
        "type": "doc",
        "content": content_nodes,
    });
    let revision_disposition = preflight_document_write_revision(
        &document_id,
        &title,
        &desired_tiptap_json,
        &comments,
        existing_record.as_ref(),
        &room,
        payload.get("expectedRevision"),
    )
    .await?;

    if revision_disposition == DocumentWriteRevisionDisposition::Apply {
        let mut content_nodes = desired_tiptap_json
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        room.update_doc(|_doc, txn| {
            // doc.get_or_insert_* opens its own transaction and panics while one
            // is live (yrs 0.27) — use the in-transaction variants.
            let fragment = txn.get_or_insert_xml_fragment("content");
            // Block-id stability across rewrites: a `write_document(content=…)`
            // wipes + rebuilds the whole fragment, and the content-string parser
            // mints FRESH random ids. That silently invalidates any block id a
            // caller obtained before the write (a subsequent edit_block_text on
            // the old id fails "Block not found"). Reuse the prior fragment's
            // ids POSITIONALLY so an in-place edit (same block structure,
            // changed text) keeps stable ids. Only on the parsed-content path;
            // ids carried in `tiptapJson`/`blocks` payloads are left untouched.
            if from_parsed_content {
                let existing_ids = block_ops::block_ids_in_fragment(txn, &fragment);
                reuse_block_ids_positionally(&mut content_nodes, &existing_ids);
            }
            let len = fragment.len(txn);
            if len > 0 {
                fragment.remove_range(txn, 0, len);
            }
            builder::append_nodes(txn, &fragment, &content_nodes);
            if !comments.is_empty() {
                let comments_map = txn.get_or_insert_map("comments");
                for (comment_id, data) in &comments {
                    comments_map.insert(txn, comment_id.as_str(), builder::json_to_any(data));
                }
            }
            Ok(())
        })
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    }
    #[cfg(test)]
    if let Err(error) = maybe_fail_document_write_after_hot_for_test(&operation.operation_id) {
        if recreation.is_some() {
            registry.evict_room(&room_key);
        }
        return Err(error);
    }

    // Materialize projections from the post-write doc state and persist via
    // the same save_document path the desktop frontend uses.
    let record_result = persist_room_document_with_expected_revision(
        app,
        &graph_id,
        &document_id,
        &title,
        &room,
        &operation.operation_id,
        payload.get("expectedRevision"),
        recreation
            .as_ref()
            .map(|tombstone| tombstone.deletion_id.as_str()),
    )
    .await;
    let record_value = match record_result {
        Ok(record) => record,
        Err(error) => {
            if recreation.is_some() {
                registry.evict_room(&room_key);
            }
            return Err(ApplyOperationError::retryable_after_hot_commit(error));
        }
    };

    #[cfg(test)]
    if recreation.is_some() {
        if let Err(error) =
            maybe_fail_document_recreation_before_workspace_for_test(&operation.operation_id)
        {
            registry.evict_room(&room_key);
            return Err(error);
        }
    }

    // Workspace entry upsert (port of the createWorkspaceDocument call).
    let workspace_result = super::workspace_ops::upsert_document_entry(
        app,
        &graph_id,
        &json!({
            "documentId": document_id,
            "title": title,
            "parentId": parent_id,
            "order": order,
            "updatedAt": operation.enqueue_timestamp.parse::<f64>().ok(),
            "readOnly": payload.get("readOnly").and_then(Value::as_bool).unwrap_or(false),
        }),
        &operation.operation_id,
    )
    .await;
    let workspace = match workspace_result {
        Ok(workspace) => workspace,
        Err(error) => {
            if recreation.is_some() {
                registry.evict_room(&room_key);
            }
            return Err(ApplyOperationError::retryable_after_hot_commit(error));
        }
    };

    // The fresh document sidecar/projection and workspace membership are now
    // both durable. Clear only the fence observed before recreation: a newer
    // concurrent delete replaces the UUID and remains authoritative.
    if let Some(tombstone) = recreation {
        let cleared = crate::document_tombstone_store::clear_document_tombstone_if_matches(
            &graph_dir,
            &document_id,
            &tombstone.deletion_id,
        )
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        if !cleared {
            return Err(ApplyOperationError::retryable_after_hot_commit(format!(
                "document tombstone disappeared before recreation commit: {document_id}"
            )));
        }
    }

    let blocks = record_value
        .get("blocks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(json!({
        "documentId": document_id,
        "id": document_id,
        "title": title,
        "parentId": parent_id,
        "workspace": workspace,
        "blockCount": blocks.len(),
        "rdfTripleCount": record_value.get("rdfTripleCount").cloned().unwrap_or(json!(0)),
        "blockIds": blocks
            .iter()
            .filter_map(|b| b.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        "sourceFormat": source_format,
        "warnings": warnings,
    }))
}

/// Top-level TipTap node types that carry a `data-block-id` (mirrors
/// `content_parse::BLOCK_ID_TYPES`, the set `ensure_block_ids` stamps). Only
/// these participate in positional id reuse.
const BLOCK_ID_NODE_TYPES: &[&str] = &[
    "paragraph",
    "heading",
    "listItem",
    "blockquote",
    "codeBlock",
    "horizontalRule",
    "image",
    "mathBlock",
];

/// Overwrite each freshly-minted top-level block id with the prior fragment's
/// id at the SAME position, so an in-place rewrite (same block structure,
/// changed text) keeps stable ids. Blocks past the old length keep their fresh
/// ids (genuinely new content). Non-block-id node types are skipped on both
/// sides so the positional alignment matches the parser's own id assignment.
///
/// This only fires on the parsed-`content`-string path, where every id was
/// random and meaningless anyway — so reuse never clobbers a caller-meant id.
fn reuse_block_ids_positionally(content_nodes: &mut [Value], existing_ids: &[String]) {
    if existing_ids.is_empty() {
        return;
    }
    let mut next_existing = 0usize;
    for node in content_nodes.iter_mut() {
        let node_type = node
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !BLOCK_ID_NODE_TYPES.contains(&node_type.as_str()) {
            continue;
        }
        let Some(existing_id) = existing_ids.get(next_existing) else {
            break; // ran out of prior ids — remaining blocks keep fresh ids
        };
        next_existing += 1;
        if let Some(obj) = node.as_object_mut() {
            let attrs = obj
                .entry("attrs".to_string())
                .or_insert_with(|| json!({}));
            if let Some(attrs_obj) = attrs.as_object_mut() {
                attrs_obj.insert("data-block-id".into(), json!(existing_id));
                // The parser may also have stamped a legacy `blockId`; keep the
                // two faces consistent (read_block_id prefers data-block-id).
                if attrs_obj.contains_key("blockId") {
                    attrs_obj.insert("blockId".into(), json!(existing_id));
                }
            }
        }
    }
}

#[cfg(test)]
mod block_id_reuse_tests {
    use super::*;

    fn block(node_type: &str, id: &str, text: &str) -> Value {
        json!({
            "type": node_type,
            "attrs": { "data-block-id": id },
            "content": [{ "type": "text", "text": text }],
        })
    }

    fn id_of(node: &Value) -> Option<&str> {
        node.get("attrs")
            .and_then(|a| a.get("data-block-id"))
            .and_then(Value::as_str)
    }

    /// The regression: an in-place rewrite (same block structure, changed text)
    /// must reuse the prior fragment's ids positionally so a block id held by a
    /// caller before the write survives it (else edit_block_text → "Block not
    /// found"). Fresh-minted ids are replaced by the existing ids at each
    /// position.
    #[test]
    fn reuses_existing_ids_positionally_on_same_structure() {
        let existing = vec![
            "block-OLD0".to_string(),
            "block-OLD1".to_string(),
            "block-OLD2".to_string(),
        ];
        let mut nodes = vec![
            block("heading", "block-fresh0", "Title v2"),
            block("paragraph", "block-fresh1", "Edited paragraph."),
            block("paragraph", "block-fresh2", "Another edit."),
        ];
        reuse_block_ids_positionally(&mut nodes, &existing);
        assert_eq!(id_of(&nodes[0]), Some("block-OLD0"));
        assert_eq!(id_of(&nodes[1]), Some("block-OLD1"));
        assert_eq!(id_of(&nodes[2]), Some("block-OLD2"));
    }

    /// Growth: the prefix reuses prior ids; blocks past the old length keep
    /// their freshly-minted ids (genuinely new content).
    #[test]
    fn appended_blocks_keep_fresh_ids() {
        let existing = vec!["block-OLD0".to_string(), "block-OLD1".to_string()];
        let mut nodes = vec![
            block("paragraph", "block-fresh0", "one"),
            block("paragraph", "block-fresh1", "two"),
            block("paragraph", "block-fresh2", "three (new)"),
            block("paragraph", "block-fresh3", "four (new)"),
        ];
        reuse_block_ids_positionally(&mut nodes, &existing);
        assert_eq!(id_of(&nodes[0]), Some("block-OLD0"));
        assert_eq!(id_of(&nodes[1]), Some("block-OLD1"));
        assert_eq!(id_of(&nodes[2]), Some("block-fresh2"));
        assert_eq!(id_of(&nodes[3]), Some("block-fresh3"));
    }

    /// Shrink: only the surviving prefix reuses ids; no panic, no leftover.
    #[test]
    fn shrink_keeps_prefix_ids() {
        let existing = vec![
            "block-OLD0".to_string(),
            "block-OLD1".to_string(),
            "block-OLD2".to_string(),
            "block-OLD3".to_string(),
        ];
        let mut nodes = vec![block("paragraph", "block-fresh0", "only one now")];
        reuse_block_ids_positionally(&mut nodes, &existing);
        assert_eq!(id_of(&nodes[0]), Some("block-OLD0"));
    }

    /// First write (no prior fragment): nothing to reuse, fresh ids untouched.
    #[test]
    fn no_existing_ids_is_a_noop() {
        let mut nodes = vec![block("paragraph", "block-fresh0", "hello")];
        reuse_block_ids_positionally(&mut nodes, &[]);
        assert_eq!(id_of(&nodes[0]), Some("block-fresh0"));
    }

    /// Non-block-id node types are skipped on the payload side so positional
    /// alignment matches the parser's own id assignment (the parser only stamps
    /// BLOCK_ID_TYPES). A stray inline-ish node between blocks must not consume
    /// an existing id slot.
    #[test]
    fn skips_non_block_id_node_types() {
        let existing = vec!["block-OLD0".to_string(), "block-OLD1".to_string()];
        let mut nodes = vec![
            block("paragraph", "block-fresh0", "para 0"),
            json!({ "type": "text", "text": "stray inline" }),
            block("paragraph", "block-fresh1", "para 1"),
        ];
        reuse_block_ids_positionally(&mut nodes, &existing);
        assert_eq!(id_of(&nodes[0]), Some("block-OLD0"));
        // the stray non-block node is untouched and consumed no id slot
        assert_eq!(id_of(&nodes[1]), None);
        assert_eq!(id_of(&nodes[2]), Some("block-OLD1"));
    }

    /// A legacy `blockId` attr is kept consistent with `data-block-id` so
    /// read_block_id (which prefers data-block-id) and any blockId reader agree.
    #[test]
    fn keeps_legacy_block_id_attr_consistent() {
        let existing = vec!["block-OLD0".to_string()];
        let mut nodes = vec![json!({
            "type": "paragraph",
            "attrs": { "data-block-id": "block-fresh0", "blockId": "block-fresh0" },
            "content": [{ "type": "text", "text": "x" }],
        })];
        reuse_block_ids_positionally(&mut nodes, &existing);
        assert_eq!(id_of(&nodes[0]), Some("block-OLD0"));
        assert_eq!(
            nodes[0]["attrs"]["blockId"].as_str(),
            Some("block-OLD0"),
            "legacy blockId must track data-block-id"
        );
    }
}

/// Port of tiptapJsonFromBlocks: blocks array → TipTap doc JSON.
pub(crate) fn tiptap_json_from_blocks(value: Option<&Value>) -> Value {
    let blocks = value.and_then(Value::as_array).cloned().unwrap_or_default();
    if blocks.is_empty() {
        // createEmptyTipTapDocument: single empty paragraph
        return json!({ "type": "doc", "content": [{ "type": "paragraph" }] });
    }
    json!({
        "type": "doc",
        "content": blocks
            .iter()
            .map(block_to_tiptap_node)
            .collect::<Result<Vec<_>, String>>()
            .unwrap_or_default(),
    })
}

/// Port of blockToTipTapNode (native-local-runtime.ts:3862).
pub(crate) fn block_to_tiptap_node(value: &Value) -> Result<Value, String> {
    let block = obj(value);
    let block_id = str_field(&block, "id").ok_or_else(|| {
        "block.insert: every block in the \"blocks\" array must carry an \"id\" field; supply IDs at the enqueue path".to_string()
    })?;
    let block_type = str_field(&block, "type").unwrap_or_else(|| "paragraph".to_string());
    let content = str_field(&block, "content").unwrap_or_default();
    let mut attrs = Map::new();
    attrs.insert("data-block-id".into(), json!(block_id));
    let text_content = inline_tiptap_content_from_block(&block, &content);

    let with_content = |node_type: &str, attrs: Map<String, Value>| -> Value {
        let mut node = Map::new();
        node.insert("type".into(), json!(node_type));
        node.insert("attrs".into(), Value::Object(attrs));
        if let Some(ref content) = text_content {
            node.insert("content".into(), content.clone());
        }
        Value::Object(node)
    };

    Ok(match block_type.as_str() {
        "heading" => {
            attrs.insert(
                "level".into(),
                json!(num_field(&block, "level").map(|n| n as i64).unwrap_or(2)),
            );
            with_content("heading", attrs)
        }
        "bullet" | "numbered" | "todo" => {
            attrs.insert(
                "listType".into(),
                json!(match block_type.as_str() {
                    "numbered" => "ordered",
                    "todo" => "task",
                    _ => "bullet",
                }),
            );
            if block_type == "todo" {
                attrs.insert(
                    "checked".into(),
                    json!(block
                        .get("checked")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)),
                );
            }
            with_content("listItem", attrs)
        }
        "quote" => with_content("blockquote", attrs),
        "code" => {
            attrs.insert("language".into(), json!(str_field(&block, "language")));
            with_content("codeBlock", attrs)
        }
        "divider" => {
            let mut node = Map::new();
            node.insert("type".into(), json!("horizontalRule"));
            node.insert("attrs".into(), Value::Object(attrs));
            Value::Object(node)
        }
        _ => with_content("paragraph", attrs),
    })
}

struct NormalizedMark {
    mark_type: String,
    start: usize,
    end: usize,
    attrs: Map<String, Value>,
}

/// Port of inlineTipTapContentFromBlock + normalizeBlockMark +
/// tiptapMarkFromBlockMark (offsets are JS string-index semantics; garden
/// block content is overwhelmingly BMP text where char counts align).
fn inline_tiptap_content_from_block(block: &Map<String, Value>, content: &str) -> Option<Value> {
    if content.is_empty() {
        return None;
    }
    let chars: Vec<char> = content.chars().collect();
    let content_len = chars.len();
    let marks: Vec<NormalizedMark> = block
        .get("marks")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|m| normalize_block_mark(m, content_len))
                .collect()
        })
        .unwrap_or_default();
    if marks.is_empty() {
        return Some(json!([{ "type": "text", "text": content }]));
    }

    let mut boundaries: Vec<usize> = vec![0, content_len];
    for mark in &marks {
        boundaries.push(mark.start);
        boundaries.push(mark.end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut nodes: Vec<Value> = Vec::new();
    for window in boundaries.windows(2) {
        let (start, end) = (window[0], window[1]);
        if end <= start {
            continue;
        }
        let text: String = chars[start..end].iter().collect();
        if text.is_empty() {
            continue;
        }
        let active: Vec<&NormalizedMark> = marks
            .iter()
            .filter(|m| m.start < end && m.end > start)
            .collect();
        if let Some(wiki) = active.iter().find(|m| m.mark_type == "wikilink") {
            nodes.push(json!({
                "type": "wikilink",
                "attrs": {
                    "targetDocId": wiki.attrs.get("targetDocId"),
                    "targetBlockId": wiki.attrs.get("targetBlockId"),
                    "targetGraphId": wiki.attrs.get("targetGraphId"),
                    "label": wiki.attrs.get("label").and_then(Value::as_str).unwrap_or(&text),
                    "blockPreview": wiki.attrs.get("blockPreview").and_then(Value::as_str).unwrap_or(&text),
                    "wireId": wiki.attrs.get("wireId"),
                },
            }));
            continue;
        }
        let tiptap_marks: Vec<Value> = active
            .iter()
            .filter_map(|m| tiptap_mark_from_block_mark(m))
            .collect();
        let mut node = Map::new();
        node.insert("type".into(), json!("text"));
        node.insert("text".into(), json!(text));
        if !tiptap_marks.is_empty() {
            node.insert("marks".into(), Value::Array(tiptap_marks));
        }
        nodes.push(Value::Object(node));
    }
    if nodes.is_empty() {
        return Some(json!([{ "type": "text", "text": content }]));
    }
    Some(Value::Array(nodes))
}

fn normalize_block_mark(value: &Value, content_len: usize) -> Option<NormalizedMark> {
    let mark = obj(value);
    let mark_type = str_field(&mark, "type")
        .or_else(|| str_field(&mark, "markType"))
        .or_else(|| str_field(&mark, "mark_type"))?;
    let start = num_field(&mark, "start")?;
    let end = num_field(&mark, "end")?;
    let bounded_start = (start.trunc().max(0.0) as usize).min(content_len);
    let bounded_end = (end.trunc().max(0.0) as usize).min(content_len);
    if bounded_end <= bounded_start {
        return None;
    }
    let pick = |keys: &[&str]| -> Value {
        for key in keys {
            if let Some(v) = str_field(&mark, key) {
                return json!(v);
            }
        }
        Value::Null
    };
    let mut attrs = Map::new();
    attrs.insert("href".into(), pick(&["href", "url"]));
    attrs.insert("target".into(), pick(&["target"]));
    attrs.insert(
        "targetDocId".into(),
        pick(&["targetDocId", "target_doc_id"]),
    );
    attrs.insert(
        "targetBlockId".into(),
        pick(&["targetBlockId", "target_block_id"]),
    );
    attrs.insert(
        "targetGraphId".into(),
        pick(&["targetGraphId", "target_graph_id"]),
    );
    attrs.insert("label".into(), pick(&["label"]));
    attrs.insert(
        "blockPreview".into(),
        pick(&["blockPreview", "block_preview"]),
    );
    attrs.insert(
        "commentId".into(),
        pick(&["commentId", "comment_id", "annotationId", "annotation_id"]),
    );
    attrs.insert("wireId".into(), pick(&["wireId", "wire_id"]));
    Some(NormalizedMark {
        mark_type,
        start: bounded_start,
        end: bounded_end,
        attrs,
    })
}

fn tiptap_mark_from_block_mark(mark: &NormalizedMark) -> Option<Value> {
    match mark.mark_type.as_str() {
        "bold" | "italic" | "strike" | "code" | "highlight" => {
            Some(json!({ "type": mark.mark_type }))
        }
        "link" => {
            let href = mark.attrs.get("href").and_then(Value::as_str)?;
            Some(
                json!({ "type": "link", "attrs": { "href": href, "target": mark.attrs.get("target") } }),
            )
        }
        "comment" | "commentMark" => {
            let comment_id = mark.attrs.get("commentId").and_then(Value::as_str)?;
            Some(json!({ "type": "commentMark", "attrs": { "commentId": comment_id } }))
        }
        "wire" | "wireMark" => {
            let wire_id = mark.attrs.get("wireId").and_then(Value::as_str)?;
            Some(json!({ "type": "wireMark", "attrs": { "wireId": wire_id } }))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared payload helpers (JS coercion ports used by the composition handlers)
// ---------------------------------------------------------------------------

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

/// JS truthiness for a JSON value.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number
            .as_f64()
            .map(|n| n != 0.0 && !n.is_nan())
            .unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Port of booleanValue.
fn js_boolean(value: Option<&Value>, fallback: bool) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => match text.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            _ => fallback,
        },
        _ => fallback,
    }
}

/// Port of stringArrayValue.
fn js_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| js_string_value(Some(item)))
                .collect()
        })
        .unwrap_or_default()
}

/// Integral floats serialize as JSON integers (matching JSON.stringify).
fn json_number(number: f64) -> Value {
    if number.is_finite() && number.fract() == 0.0 && number.abs() < 9.007_199_254_740_992e15 {
        json!(number as i64)
    } else {
        json!(number)
    }
}

fn opt_string(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}

/// yrs Any → JSON (the document comments map stores plain Any values).
fn any_json(any: &Any) -> Value {
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(flag) => json!(flag),
        Any::Number(number) => json_number(*number),
        Any::BigInt(number) => json!(number),
        Any::String(text) => json!(text.as_ref()),
        Any::Buffer(bytes) => json!(bytes.as_ref()),
        Any::Array(items) => Value::Array(items.iter().map(any_json).collect()),
        Any::Map(entries) => {
            let mut object = Map::new();
            for (key, value) in entries.iter() {
                object.insert(key.clone(), any_json(value));
            }
            Value::Object(object)
        }
    }
}

/// Port of encodeURIComponent (UTF-8 percent-encoding; RFC 2396 unreserved
/// set plus `!'()*` stay literal, exactly like the JS builtin).
pub fn encode_uri_component(value: &str) -> String {
    const KEEP: &[u8] = b"-_.!~*'()";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || KEEP.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Port of the ingestMarkdownOriginal title fallback:
/// `filename.replace(/\.[^.]+$/, '').replace(/[-_]+/g, ' ')`.
pub fn filename_stem_title(filename: &str) -> String {
    let stem = match filename.rfind('.') {
        // /\.[^.]+$/ needs at least one character after the final dot.
        Some(index) if index + 1 < filename.len() => &filename[..index],
        _ => filename,
    };
    let mut out = String::with_capacity(stem.len());
    let mut in_run = false;
    for ch in stem.chars() {
        if ch == '-' || ch == '_' {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(ch);
            in_run = false;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Shared persistence tail (the saveDocumentSnapshot equivalent used by the
// composition handlers; document_write keeps its inline original)
// ---------------------------------------------------------------------------

/// Materialize the room doc and persist via save_document (record + RDF).
/// Returns the saved record as JSON.
pub(crate) async fn persist_room_document(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    title: &str,
    room: &super::rooms::Room,
    operation_id: &str,
) -> Result<Value, String> {
    persist_room_document_with_expected_revision(
        app,
        graph_id,
        document_id,
        title,
        room,
        operation_id,
        None,
        None,
    )
    .await
}

/// Flush one dirty hosted document through the same serialized gate used by
/// normal document/block operations. `None` means another waiter already
/// persisted an equal-or-newer room epoch.
pub(crate) async fn flush_room_document_if_dirty(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    title: &str,
    room: &super::rooms::Room,
    operation_id: &str,
) -> Result<Option<Value>, String> {
    let _projection_guard = room.lock_projection_flush().await;
    if !room.needs_projection_flush() {
        return Ok(None);
    }
    persist_room_document_locked(
        app,
        graph_id,
        document_id,
        title,
        room,
        operation_id,
        None,
        None,
    )
    .await
    .map(Some)
}

/// Rebuild only the RDF face for one persisted document Y.Doc.
///
/// A normal flush owns revision/history/graph-touch tails. A projection replay
/// must own none of those effects: it verifies that the existing cold record
/// still describes the authoritative Y.Doc, then rematerializes RDF from that
/// record without manufacturing a new mutation timestamp or revision.
pub(crate) async fn rebuild_room_document_projection(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    title: &str,
    room: &super::rooms::Room,
    operation_id: &str,
) -> Result<bool, String> {
    let _projection_guard = room.lock_projection_flush().await;
    if !room.needs_projection_flush() {
        return Ok(false);
    }
    let materialized =
        materialize_room_document(graph_id, document_id, title, room, operation_id, None).await?;
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let manifest = document_dir(&graph_dir, document_id)?.join("document.json");
    if !manifest.is_file() {
        return Err(format!(
            "document projection rebuild has Y.Doc but no record: {graph_id}/{document_id}"
        ));
    }
    let record = read_document_record(&graph_dir, &manifest)?;
    let record_value = serde_json::to_value(&record)
        .map_err(|error| format!("serialize document rebuild record: {error}"))?;
    if !projection_semantics_match(&record_value, &materialized.candidate)? {
        return Err(format!(
            "document projection rebuild source mismatch: {graph_id}/{document_id}"
        ));
    }
    let store = crate::rdf_service::open_graph_store(&graph_dir)?;
    crate::document_meaningful_object::reconcile_document_record(&store, &record)?;
    crate::pdf_source::reconcile(&store, &record)?;
    room.mark_projection_persisted(materialized.projection_epoch);
    Ok(true)
}

async fn persist_room_document_with_expected_revision(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    title: &str,
    room: &super::rooms::Room,
    operation_id: &str,
    expected_revision: Option<&Value>,
    expected_deletion_id: Option<&str>,
) -> Result<Value, String> {
    let _projection_guard = room.lock_projection_flush().await;
    persist_room_document_locked(
        app,
        graph_id,
        document_id,
        title,
        room,
        operation_id,
        expected_revision,
        expected_deletion_id,
    )
    .await
}

pub(super) struct MaterializedRoomDocument {
    projection_epoch: u64,
    candidate: Value,
    save_input: SaveDocumentInput,
}

pub(super) async fn materialize_room_document(
    graph_id: &str,
    document_id: &str,
    title: &str,
    room: &super::rooms::Room,
    operation_id: &str,
    expected_revision: Option<&Value>,
) -> Result<MaterializedRoomDocument, String> {
    let (projection_epoch, (snapshot, tiptap_xml, ydoc_state)) = room
        .with_doc_version(|doc| {
            let snapshot = projection::materialize_ydoc(doc, document_id);
            let tiptap_xml = projection::ydoc_to_tiptap_xml(doc);
            let txn = doc.transact();
            let ydoc_state = txn.encode_state_as_update_v1(&yrs::StateVector::default());
            (snapshot, tiptap_xml, ydoc_state)
        })
        .await;
    let ydoc_b64 = base64::engine::general_purpose::STANDARD.encode(ydoc_state);
    let candidate = json!({
        "graphId": graph_id,
        "documentId": document_id,
        "title": title,
        "body": snapshot.body,
        "tiptapXml": tiptap_xml,
        "tiptapJson": snapshot.tiptap_json,
        "ydocUpdateBase64": ydoc_b64,
        "tree": snapshot.tree_json,
        "blocks": snapshot.blocks_json,
        "traceOperationId": operation_id,
        "expectedRevision": expected_revision.cloned(),
    });
    let save_input: SaveDocumentInput = serde_json::from_value(candidate.clone())
        .map_err(|error| format!("assemble save_document input: {error}"))?;
    Ok(MaterializedRoomDocument {
        projection_epoch,
        candidate,
        save_input,
    })
}

fn canonical_projection_comments(value: &Value) -> Result<Value, String> {
    let Some(encoded) = value
        .get("ydocUpdateBase64")
        .and_then(Value::as_str)
        .filter(|encoded| !encoded.is_empty())
    else {
        return Ok(json!({}));
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("decode document Y.Doc state for comment comparison: {error}"))?;
    let update = Update::decode_v1(&bytes)
        .map_err(|error| format!("decode document Y.Doc update for comment comparison: {error}"))?;
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(update).map_err(|error| {
            format!("apply document Y.Doc update for comment comparison: {error}")
        })?;
    }
    let txn = doc.transact();
    let Some(comments) = txn.get_map("comments") else {
        return Ok(json!({}));
    };
    let mut ids = comments.keys(&txn).map(str::to_string).collect::<Vec<_>>();
    ids.sort();
    let mut canonical = Map::new();
    for id in ids {
        match comments.get(&txn, &id) {
            Some(Out::Any(comment)) => {
                canonical.insert(id, any_json(&comment));
            }
            Some(_) => {
                return Err(format!("document comment {id} is not a plain JSON value"));
            }
            None => {}
        }
    }
    Ok(Value::Object(canonical))
}

fn projection_semantics_match(record: &Value, candidate: &Value) -> Result<bool, String> {
    // `tiptapJson` is the authoritative structural projection. `tiptapXml`,
    // `tree`, and `blocks` are derived caches and are not stable identity
    // keys: fallback block ids and mark ids intentionally mirror the frontend's
    // random UUID generation. Comparing those caches makes a persisted Y.Doc
    // look edited after process restart even when its TipTap content is byte-for-
    // byte identical. Body is retained as a defensive semantic cross-check.
    // Comments are authoritative non-TipTap semantics, but raw CRDT update
    // bytes are not: equal visible state can have different Yjs histories.
    let content_matches = ["title", "body", "tiptapJson"]
        .into_iter()
        .all(|field| record.get(field) == candidate.get(field));
    if !content_matches {
        return Ok(false);
    }
    Ok(canonical_projection_comments(record)? == canonical_projection_comments(candidate)?)
}

pub(super) fn reconcile_matching_document_projection(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    candidate: &Value,
) -> Result<Option<Value>, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let manifest = document_dir(&graph_dir, document_id)?.join("document.json");
    if !manifest.is_file() {
        return Ok(None);
    }
    let raw_record: DocumentRecord =
        crate::storage::read_json(&manifest).map_err(|error| error.to_string())?;
    let record = read_document_record(&graph_dir, &manifest)?;
    let raw_record_value = serde_json::to_value(&raw_record)
        .map_err(|error| format!("serialize raw document record: {error}"))?;
    let record_value = serde_json::to_value(&record)
        .map_err(|error| format!("serialize existing document record: {error}"))?;
    let fields_match = projection_semantics_match(&raw_record_value, candidate)?;

    if let Some(expected) = candidate.get("expectedRevision").and_then(Value::as_u64) {
        // Preserve save_document's optimistic-concurrency contract. Equality at
        // expected+1 is the already-applied replay; equality at expected still
        // needs a real revision increment.
        if record.revision == expected {
            return Ok(None);
        }
        if record.revision != expected.saturating_add(1) {
            return Err(format!(
                "revision conflict: expected {expected}, actual {}",
                record.revision,
            ));
        }
        if !fields_match {
            return Err(format!(
                "revision conflict: expected {expected}, actual {}; replay content differs",
                record.revision,
            ));
        }
    } else if !fields_match {
        return Ok(None);
    }

    // A prior attempt may have written document.json and then failed during
    // RDF, history, graph-touch, or the final tail marker. Repair all stages
    // under the same exact document revision; never route this replay through
    // save_document, which would manufacture another revision.
    crate::document_persistence_service::ensure_document_persistence_tail(&graph_dir, &record)?;
    Ok(Some(record_value))
}

async fn persist_room_document_locked(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    title: &str,
    room: &super::rooms::Room,
    operation_id: &str,
    expected_revision: Option<&Value>,
    expected_deletion_id: Option<&str>,
) -> Result<Value, String> {
    let materialized = materialize_room_document(
        graph_id,
        document_id,
        title,
        room,
        operation_id,
        expected_revision,
    )
    .await?;
    persist_materialized_room_document_with_tombstone_fence(
        app,
        graph_id,
        document_id,
        room,
        materialized,
        expected_deletion_id,
    )
}

pub(super) fn persist_materialized_room_document(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    room: &super::rooms::Room,
    materialized: MaterializedRoomDocument,
) -> Result<Value, String> {
    persist_materialized_room_document_with_tombstone_fence(
        app,
        graph_id,
        document_id,
        room,
        materialized,
        None,
    )
}

fn persist_materialized_room_document_with_tombstone_fence(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    room: &super::rooms::Room,
    materialized: MaterializedRoomDocument,
    expected_deletion_id: Option<&str>,
) -> Result<Value, String> {
    let record_value = if let Some(expected_deletion_id) = expected_deletion_id {
        let graph_dir = existing_graph_dir(app, graph_id)?;
        crate::document_tombstone_store::require_document_tombstone_matches(
            &graph_dir,
            document_id,
            expected_deletion_id,
        )?;
        let record = crate::document_persistence_service::save_document_for_recreation(
            app.clone(),
            materialized.save_input,
            expected_deletion_id,
        )?;
        serde_json::to_value(&record).map_err(|error| format!("serialize record: {error}"))?
    } else {
        match reconcile_matching_document_projection(
            app,
            graph_id,
            document_id,
            &materialized.candidate,
        )? {
            Some(record) => record,
            None => {
                let record = crate::document_persistence_service::save_document_with_lease(
                    app.clone(),
                    materialized.save_input,
                )?;
                serde_json::to_value(&record)
                    .map_err(|error| format!("serialize record: {error}"))?
            }
        }
    };
    room.mark_projection_persisted(materialized.projection_epoch);
    Ok(record_value)
}

/// Propagate the authoritative workspace title into an existing document's
/// cold record. Workspace RDF is reconciled from the same snapshot before this
/// helper runs; the per-document projection intentionally does not own
/// `dcterms:title`. This is the Rust analogue of desktop
/// `syncWorkspaceDocumentsFromYDoc`'s title branch. It shares the room's
/// projection gate with content saves, so a concurrent content mutation either
/// lands before this save or observes the renamed record and lands after it.
pub(crate) async fn sync_room_document_title(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    title: &str,
    room: &super::rooms::Room,
    operation_id: &str,
) -> Result<bool, String> {
    let _projection_guard = room.lock_projection_flush().await;
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let manifest = document_dir(&graph_dir, document_id)?.join("document.json");
    if !manifest.is_file() {
        return Ok(false);
    }
    let record = read_document_record(&graph_dir, &manifest)?;
    if record.title == title {
        return Ok(false);
    }

    // Same root defect e4f819e fixed in rooms.rs's room hydration and
    // graph_duplicate_storage.rs's `rewrite_duplicate_document_ydoc`:
    // `document_sidecar_store::write_ydoc_update` (32fa25b) now persists a
    // canonical encoded-empty Y.Doc (non-zero bytes) instead of a zero-byte
    // file for a caller that writes no ydoc content — which is every
    // document ever saved through the file-only `save_document` path (this
    // function never touches the Y.Doc for such a document). This function's
    // `!record.ydoc_update_base64.is_empty()` check (aa1463f, predates
    // 32fa25b) used to be a valid signal that "this record's Y.Doc already
    // carries real content": a genuinely untouched legacy record's inline
    // field really was an empty string. Since `read_document_record`
    // canonicalizes every sidecar read to a non-empty base64 string (real
    // content OR canonical-empty), the raw string check now reads true for
    // EVERY document with a sidecar file at all. Decode and ask
    // `yrs::Update::is_empty()` (blocks + delete-set both empty) instead — a
    // byte-level check does NOT work here, same reasoning as the other two
    // fixes: Y.Doc encodes a random per-instance client id even with zero
    // blocks. Treating canonical-empty content as "real" would route a bare
    // workspace title rename of a live-but-untouched room (hydration
    // correctly left its in-memory Doc blank, per the rooms.rs fix) through
    // `persist_room_document_locked`, which materializes that blank Doc and
    // clobbers the file-authored body/tiptapXml/tree with an empty
    // projection.
    let ydoc_has_content = if record.ydoc_update_base64.is_empty() {
        false
    } else {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&record.ydoc_update_base64)
            .map_err(|error| format!("decode document Y.Doc state for title sync: {error}"))?;
        !Update::decode_v1(&bytes)
            .map_err(|error| format!("decode document Y.Doc update for title sync: {error}"))?
            .is_empty()
    };

    if room.needs_projection_flush() || ydoc_has_content {
        persist_room_document_locked(
            app,
            graph_id,
            document_id,
            title,
            room,
            operation_id,
            None,
            None,
        )
        .await?;
        return Ok(true);
    }

    // Legacy records — and now, records whose Y.Doc carries no real content —
    // preserve their cached content fields verbatim while changing only the
    // title.
    save_record_with_workspace_title(app, &record, title, operation_id)?;
    let (epoch, ()) = room.with_doc_version(|_| ()).await;
    room.mark_projection_persisted(epoch);
    Ok(true)
}

/// Cold-path analogue of `sync_room_document_title` for a document whose room
/// is NOT currently live. Writes the authoritative workspace title into the
/// cold record (document.json + persistence tail) via the same record
/// round-trip save used for legacy records — no room hydration, no Y.Doc
/// decode. Hydrating a room here is what let a hydrated-dirty workspace's
/// first flush mass-decode every mismatched document's full update history
/// (retained forever by the registry) and OOM a fresh cell.
///
/// Invariant this relies on: the document Y.Doc carries no internal title —
/// `materialize_room_document` takes the title as a parameter and the
/// per-document projection intentionally does not own `dcterms:title` (see
/// `sync_room_document_title` above). When the room is opened lazily later,
/// its next flush re-reads the title from the live workspace room or this
/// manifest (`flush_ops::live_workspace_title` → `stored_document_title`), so
/// titles for cold rooms reconcile via the manifest now and room-internal
/// state converges on first open without any decode here.
pub(super) fn sync_cold_document_title(
    app: &AppHandle,
    record: &DocumentRecord,
    title: &str,
    operation_id: &str,
) -> Result<(), String> {
    save_record_with_workspace_title(app, record, title, operation_id)
}

/// Persist `record` with only its title replaced by the authoritative
/// workspace `title`, preserving every cached content field verbatim. The
/// record's `ydoc_update_base64` (hydrated from the sidecar by
/// `read_document_record`) round-trips byte-identically; nothing decodes it.
fn save_record_with_workspace_title(
    app: &AppHandle,
    record: &DocumentRecord,
    title: &str,
    operation_id: &str,
) -> Result<(), String> {
    let mut input_value = serde_json::to_value(record)
        .map_err(|error| format!("serialize document title sync input: {error}"))?;
    let input = input_value
        .as_object_mut()
        .ok_or_else(|| "document title sync input is not an object".to_string())?;
    input.insert("title".to_string(), json!(title));
    input.insert("traceOperationId".to_string(), json!(operation_id));
    input.remove("revision");
    input.remove("origin");
    input.remove("providerId");
    input.remove("localPath");
    input.remove("rdfSubject");
    input.remove("createdAt");
    input.remove("updatedAt");
    input.remove("capabilities");
    input.remove("schemaVersion");
    input.remove("ydocStatePath");
    input.remove("rdfTripleCount");
    let save_input: SaveDocumentInput = serde_json::from_value(input_value)
        .map_err(|error| format!("assemble document title sync input: {error}"))?;
    crate::document_persistence_service::save_document_with_lease(app.clone(), save_input)?;
    Ok(())
}

/// Existing record title fallback used when a mutation must persist without
/// carrying a title of its own (mirrors block_ops::persist_document).
fn existing_record_title(graph_dir: &std::path::Path, document_id: &str) -> String {
    document_dir(graph_dir, document_id)
        .ok()
        .map(|dir| dir.join("document.json"))
        .filter(|manifest| manifest.exists())
        .and_then(|manifest| read_document_record(graph_dir, &manifest).ok())
        .map(|record| record.title)
        .unwrap_or_else(|| "Untitled".to_string())
}

// ---------------------------------------------------------------------------
// document.editComment (port of editDocumentComment,
// native-local-runtime.ts:2762, plus clearCommentMarks / formatBlockTextRange
// / occurrenceOffsets from block-mutations.ts)
// ---------------------------------------------------------------------------

/// Port of commentAction: default 'set', anything else than the three
/// verbs is an error.
fn comment_action(value: Option<&Value>) -> Result<&'static str, String> {
    let action = js_string_value(value).unwrap_or_else(|| "set".to_string());
    match action.as_str() {
        "set" => Ok("set"),
        "resolve" => Ok("resolve"),
        "delete" => Ok("delete"),
        _ => Err("document.editComment: action must be set, resolve, or delete".to_string()),
    }
}

/// Port of normalizeCommentData. The TS version stamps `Date.now()` for
/// updatedAt (and the createdAt fallback); cells substitute the journaled
/// operation timestamp so replays are deterministic (A2 item 11 discipline).
pub fn normalize_comment_data(value: &Value, now_ms: f64) -> Value {
    let data = value.as_object().cloned().unwrap_or_default();
    if data.is_empty() {
        return value.clone();
    }
    let mut out = data.clone();
    out.insert(
        "text".to_string(),
        json!(js_string_value(data.get("text")).unwrap_or_default()),
    );
    out.insert(
        "author".to_string(),
        json!(js_string_value(data.get("author")).unwrap_or_else(|| "MCP Agent".to_string())),
    );
    out.insert(
        "authorId".to_string(),
        json!(js_string_value(coalesce(&data, &["authorId", "author_id"]))
            .unwrap_or_else(|| "mcp-agent".to_string())),
    );
    out.insert(
        "resolved".to_string(),
        json!(data.get("resolved").map(js_truthy).unwrap_or(false)),
    );
    out.insert(
        "createdAt".to_string(),
        coalesce(&data, &["createdAt", "created_at"])
            .cloned()
            .unwrap_or_else(|| json_number(now_ms)),
    );
    out.insert("updatedAt".to_string(), json_number(now_ms));
    Value::Object(out)
}

/// Port of commentMarkId (block-mutations.ts): string marks are the id
/// itself; object marks carry commentId/comment_id/annotationId/annotation_id.
fn comment_mark_id(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Object(map) => match coalesce(
            map,
            &["commentId", "comment_id", "annotationId", "annotation_id"],
        ) {
            Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Port of readBlockText: plain text of every text segment in order.
pub(crate) fn read_block_text<T: ReadTxn>(txn: &T, element: &XmlElementRef) -> String {
    block_ops::collect_text_segments(txn, element)
        .iter()
        .map(|segment| block_ops::xml_text_content(txn, &segment.text))
        .collect()
}

/// Port of occurrenceOffsets. Offsets and lengths are UTF-16 code units
/// (JS string semantics): occurrence 0 → all, -1 → last, n ≥ 1 → the nth.
pub fn occurrence_offsets(text: &str, find: &str, occurrence: f64) -> Vec<usize> {
    if find.is_empty() {
        return Vec::new();
    }
    let text_units: Vec<u16> = text.encode_utf16().collect();
    let find_units: Vec<u16> = find.encode_utf16().collect();
    let mut offsets: Vec<usize> = Vec::new();
    let mut cursor = 0usize;
    while cursor <= text_units.len() {
        let found = if cursor + find_units.len() > text_units.len() {
            None
        } else {
            (cursor..=text_units.len() - find_units.len())
                .find(|&start| text_units[start..start + find_units.len()] == find_units[..])
        };
        let Some(offset) = found else { break };
        offsets.push(offset);
        cursor = offset + find_units.len().max(1);
    }
    if occurrence == 0.0 {
        return offsets;
    }
    if occurrence == -1.0 {
        return offsets
            .last()
            .map(|offset| vec![*offset])
            .unwrap_or_default();
    }
    let index = (occurrence.floor().max(1.0) as usize) - 1;
    offsets
        .get(index)
        .map(|offset| vec![*offset])
        .unwrap_or_default()
}

/// Port of formatBlockTextRange: apply formatting attrs to the UTF-16 range
/// [offset, offset+length) across the block's text segments. Returns the
/// number of segments formatted (`applied`).
pub(crate) fn format_block_text_range(
    txn: &mut TransactionMut<'_>,
    element: &XmlElementRef,
    offset: usize,
    length: usize,
    attrs: &Map<String, Value>,
) -> usize {
    if length == 0 {
        return 0;
    }
    let segments = block_ops::collect_text_segments(txn, element);
    let total_length: usize = segments.iter().map(|segment| segment.length).sum();
    let start = offset.min(total_length);
    let end = (start + length).min(total_length).max(start);
    let Some(format_attrs) = block_ops::to_format_attrs(attrs) else {
        return 0;
    };
    if start == end {
        return 0;
    }
    let mut applied = 0usize;
    for segment in &segments {
        let segment_end = segment.start + segment.length;
        if segment_end <= start || segment.start >= end {
            continue;
        }
        let local_start = start.saturating_sub(segment.start);
        let local_end = (end - segment.start).min(segment.length);
        if local_end <= local_start {
            continue;
        }
        // Payload offsets are UTF-16; yrs wants byte offsets.
        let content = block_ops::xml_text_content(txn, &segment.text);
        let byte_start = block_ops::utf16_to_byte_offset(&content, local_start);
        let byte_end = block_ops::utf16_to_byte_offset(&content, local_end);
        segment.text.format(
            txn,
            byte_start as u32,
            (byte_end - byte_start) as u32,
            format_attrs.clone(),
        );
        applied += 1;
    }
    applied
}

/// Port of clearCommentMarks: remove the commentMark formatting for
/// `comment_id` from every delta chunk of the element's text segments.
/// Returns the number of chunks cleared.
pub(crate) fn clear_comment_marks(
    txn: &mut TransactionMut<'_>,
    element: &XmlElementRef,
    comment_id: &str,
) -> usize {
    let mut cleared = 0usize;
    for segment in block_ops::collect_text_segments(txn, element) {
        // Two-phase: read the delta immutably, then clear matching chunks.
        // Formatting does not move text, so byte offsets stay valid.
        let mut matches: Vec<(usize, usize)> = Vec::new();
        let mut cursor = 0usize;
        for diff in segment.text.diff(txn, YChange::identity) {
            let Out::Any(Any::String(chunk)) = diff.insert else {
                continue;
            };
            let length = chunk.len();
            let mark = diff
                .attributes
                .as_deref()
                .and_then(|attrs| attrs.get("commentMark"))
                .map(any_json);
            if mark.as_ref().and_then(comment_mark_id).as_deref() == Some(comment_id) {
                matches.push((cursor, length));
            }
            cursor += length;
        }
        for (start, length) in matches {
            let mut format_attrs = Attrs::new();
            format_attrs.insert(Arc::from("commentMark"), Any::Null);
            segment
                .text
                .format(txn, start as u32, length as u32, format_attrs);
            cleared += 1;
        }
    }
    cleared
}

/// In-transaction body of document.editComment (the transactDocument
/// callback in the TS handler).
pub fn edit_comment_in_doc(
    txn: &mut TransactionMut<'_>,
    payload: &Map<String, Value>,
    action: &'static str,
    comment_id: &str,
    now: f64,
) -> Result<Value, String> {
    let fragment = txn.get_or_insert_xml_fragment("content");
    let comments_map = txn.get_or_insert_map("comments");
    let existing = match comments_map.get(&*txn, comment_id) {
        Some(Out::Any(any)) => any_json(&any).as_object().cloned().unwrap_or_default(),
        _ => Map::new(),
    };

    if action == "delete" {
        let existed = comments_map.contains_key(&*txn, comment_id);
        comments_map.remove(txn, comment_id);
        let elements: Vec<XmlElementRef> = fragment
            .children(txn)
            .filter_map(|child| match child {
                XmlOut::Element(element) => Some(element),
                _ => None,
            })
            .collect();
        let mut cleared = 0usize;
        for element in &elements {
            cleared += clear_comment_marks(txn, element, comment_id);
        }
        return Ok(json!({
            "success": true,
            "action": action,
            "commentId": comment_id,
            "deleted": existed,
            "cleared": cleared,
        }));
    }

    if action == "resolve" {
        // `value.resolved === undefined ? true : Boolean(value.resolved)`
        let resolved = match payload.get("resolved") {
            None => true,
            Some(value) => js_truthy(value),
        };
        let mut data = existing;
        data.insert("commentId".to_string(), json!(comment_id));
        data.insert("id".to_string(), json!(comment_id));
        data.insert("resolved".to_string(), json!(resolved));
        data.insert("updatedAt".to_string(), json_number(now));
        let comment = normalize_comment_data(&Value::Object(data), now);
        comments_map.insert(txn, comment_id, builder::json_to_any(&comment));
        return Ok(json!({
            "success": true,
            "action": action,
            "commentId": comment_id,
            "comment": comment,
            "resolved": resolved,
        }));
    }

    // action == "set"
    let text = js_string_value(payload.get("text"))
        .ok_or_else(|| "document.editComment: text is required for action=set".to_string())?;
    let author = js_string_value(payload.get("author"))
        .or_else(|| js_string_value(existing.get("author")))
        .unwrap_or_else(|| "MCP Agent".to_string());
    let quoted_text = js_string_value(coalesce(payload, &["quotedText", "quoted_text"]));
    let block_id = js_string_value(coalesce(payload, &["blockId", "block_id"]));
    let find = js_string_value(payload.get("find"));
    let occurrence = js_finite_number(payload.get("occurrence")).unwrap_or(1.0);
    let mut anchored = 0usize;
    let mut anchor_block_id: Option<String> = None;
    let mut effective_quoted_text = quoted_text;

    if let (Some(block_id), Some(find)) = (&block_id, &find) {
        let (element, _) = block_ops::find_block_in_fragment(txn, &fragment, block_id)
            .ok_or_else(|| format!("Block not found: {block_id}"))?;
        let block_text = read_block_text(txn, &element);
        let offsets = occurrence_offsets(&block_text, find, occurrence);
        if offsets.is_empty() {
            return Err(format!("text not found in block {block_id}: {find}"));
        }
        let mark_attrs = json!({ "commentMark": { "commentId": comment_id } })
            .as_object()
            .cloned()
            .unwrap_or_default();
        for offset in offsets {
            anchored += format_block_text_range(
                txn,
                &element,
                offset,
                block_ops::utf16_len(find),
                &mark_attrs,
            );
        }
        anchor_block_id = Some(block_id.clone());
        if effective_quoted_text.is_none() {
            effective_quoted_text = Some(find.clone());
        }
    }

    let mut data = existing.clone();
    data.insert("id".to_string(), json!(comment_id));
    data.insert("commentId".to_string(), json!(comment_id));
    data.insert("text".to_string(), json!(text));
    data.insert("author".to_string(), json!(author));
    // authorId: value.authorId ?? value.author_id ?? existing.authorId ??
    // existing.author_id (raw passthrough; normalizeCommentData re-derives).
    if let Some(author_id) = coalesce(payload, &["authorId", "author_id"])
        .or_else(|| coalesce(&existing, &["authorId", "author_id"]))
        .cloned()
    {
        data.insert("authorId".to_string(), author_id);
    }
    let block_id_entry = anchor_block_id
        .clone()
        .or_else(|| js_string_value(coalesce(&existing, &["blockId", "block_id"])));
    data.insert("blockId".to_string(), opt_string(block_id_entry));
    let quoted_entry = effective_quoted_text
        .clone()
        .or_else(|| js_string_value(coalesce(&existing, &["quotedText", "quoted_text"])));
    data.insert("quotedText".to_string(), opt_string(quoted_entry));
    // `Boolean(value.resolved ?? existing.resolved ?? false)`
    let resolved = payload
        .get("resolved")
        .filter(|value| !value.is_null())
        .or_else(|| existing.get("resolved").filter(|value| !value.is_null()))
        .map(js_truthy)
        .unwrap_or(false);
    data.insert("resolved".to_string(), json!(resolved));
    data.insert(
        "createdAt".to_string(),
        coalesce(&existing, &["createdAt", "created_at"])
            .cloned()
            .unwrap_or_else(|| json_number(now)),
    );
    data.insert("updatedAt".to_string(), json_number(now));
    let comment = normalize_comment_data(&Value::Object(data), now);
    comments_map.insert(txn, comment_id, builder::json_to_any(&comment));
    Ok(json!({
        "success": true,
        "action": action,
        "commentId": comment_id,
        "comment": comment,
        "anchored": anchored,
        "blockId": opt_string(anchor_block_id),
        "quotedText": opt_string(effective_quoted_text),
    }))
}

/// Port of editDocumentComment (native-local-runtime.ts:2762).
#[cfg(test)]
static FAIL_NEXT_EDIT_COMMENT_AFTER_HOT: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_edit_comment_after_hot_for_test(operation_id: impl Into<String>) {
    *FAIL_NEXT_EDIT_COMMENT_AFTER_HOT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(operation_id.into());
}

#[cfg(test)]
fn maybe_fail_edit_comment_after_hot_for_test(
    operation_id: &str,
) -> Result<(), ApplyOperationError> {
    let mut pending = FAIL_NEXT_EDIT_COMMENT_AFTER_HOT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending.as_deref() == Some(operation_id) {
        pending.take();
        return Err(ApplyOperationError::retryable_after_hot_commit(format!(
            "injected editComment failure after hot commit for {operation_id}"
        )));
    }
    Ok(())
}

pub(crate) async fn edit_comment(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> Result<Value, String> {
    edit_comment_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

pub(crate) async fn edit_comment_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let graph_id = operation.graph_id.clone();
    let document_id = operation
        .document_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "document.editComment: documentId is required".to_string())?;
    let payload = obj(&operation.payload);
    let action = comment_action(payload.get("action"))?;
    let comment_id = js_string_value(coalesce(&payload, &["commentId", "comment_id"]))
        .ok_or_else(|| "document.editComment: commentId is required".to_string())?;
    let now = js_finite_number(payload.get("updatedAt")).ok_or_else(|| {
        "document.editComment: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
            .to_string()
    })?;

    let graph_dir = existing_graph_dir(app, &graph_id)?;
    let registry = app.state::<RoomRegistry>();
    let room = registry
        .get_or_create(
            &format!("doc:{graph_id}:{document_id}"),
            checked_document_ydoc_state_path(&graph_dir, &document_id)?,
        )
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    const VALIDATION_PREFIX: &str = "__garden_edit_comment_validation__:";
    let result = match {
        let comment_id = comment_id.clone();
        room.update_doc(move |_doc, txn| {
            edit_comment_in_doc(txn, &payload, action, &comment_id, now)
                .map_err(|error| format!("{VALIDATION_PREFIX}{error}"))
        })
        .await
    } {
        Ok(result) => result,
        Err(error) => match error.strip_prefix(VALIDATION_PREFIX) {
            Some(validation) => return Err(ApplyOperationError::terminal(validation)),
            None => {
                // Room persistence evicts the uncommitted incarnation, so
                // retrying the same operation is safe. Never terminalize a
                // transient sidecar write failure.
                return Err(ApplyOperationError::retryable_after_hot_commit(error));
            }
        },
    };
    #[cfg(test)]
    maybe_fail_edit_comment_after_hot_for_test(&operation.operation_id)?;

    // The TS handler runs with {flush:false} and lets the editor channel's
    // debounced save persist; headless cells persist eagerly with the
    // existing record title (block_ops parity).
    let title = existing_record_title(&graph_dir, &document_id);
    persist_room_document(
        app,
        &graph_id,
        &document_id,
        &title,
        &room,
        &operation.operation_id,
    )
    .await
    .map_err(ApplyOperationError::retryable_after_hot_commit)?;

    let mut response = Map::new();
    response.insert("documentId".to_string(), json!(document_id));
    response.insert("graphId".to_string(), json!(graph_id));
    if let Value::Object(fields) = result {
        for (key, value) in fields {
            response.insert(key, value);
        }
    }
    Ok(Value::Object(response))
}

// ---------------------------------------------------------------------------
// document.liveProjection (port of liveProjection,
// native-local-runtime.ts:314; the room registry is the cell's channel pool)
// ---------------------------------------------------------------------------

pub(crate) async fn live_projection(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> Result<Value, String> {
    let payload = obj(&operation.payload);
    let document_id = operation
        .document_id
        .clone()
        .filter(|id| !id.is_empty())
        .or_else(|| str_field(&payload, "documentId"));
    let Some(document_id) = document_id else {
        return Ok(json!({ "active": false }));
    };
    crate::ids::validate_local_id(&document_id, "document_id")?;
    let graph_id = operation.graph_id.clone();
    let registry = app.state::<RoomRegistry>();
    let Some(room) = registry
        .peek(&format!("doc:{graph_id}:{document_id}"))
        .await
    else {
        return Ok(json!({ "active": false }));
    };

    let snapshot = {
        let document_id = document_id.clone();
        room.with_doc(move |doc| projection::materialize_ydoc(doc, &document_id))
            .await
    };
    let graph_dir = existing_graph_dir(app, &graph_id).ok();
    let record = graph_dir
        .as_deref()
        .and_then(|dir| super::workspace_ops::document_record_json(dir, &document_id));

    // Title chain: workspace documents map (filesystemStore equivalent),
    // then the document.json record, then 'Untitled'.
    let workspace_title = match registry.peek(&format!("workspace:{graph_id}")).await {
        Some(workspace_room) => {
            let document_id = document_id.clone();
            workspace_room
                .with_doc(move |doc| {
                    super::workspace_ops::document_title_in_workspace(doc, &document_id)
                })
                .await
        }
        None => None,
    };
    let title = workspace_title
        .or_else(|| {
            record
                .as_ref()
                .and_then(|value| value.get("title"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "Untitled".to_string());
    let rdf_triple_count = record
        .as_ref()
        .and_then(|value| value.get("rdfTripleCount"))
        .filter(|value| value.is_number())
        .cloned()
        .unwrap_or_else(|| json!(0));
    let block_count = snapshot
        .blocks_json
        .as_array()
        .map(|blocks| blocks.len())
        .unwrap_or(0);
    Ok(json!({
        "active": true,
        "blocks": snapshot.blocks_json,
        "title": title,
        "body": snapshot.body,
        "blockCount": block_count,
        "rdfTripleCount": rdf_triple_count,
    }))
}

// ---------------------------------------------------------------------------
// document.ingestMarkdownOriginal (port of ingestMarkdownOriginalDocument,
// native-local-runtime.ts:571)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IngestFailurePoint {
    AfterOriginal,
    AfterWorkspace,
    AfterHot,
    AfterTail,
}

#[cfg(test)]
static FAIL_NEXT_INGEST_STEP: std::sync::OnceLock<
    std::sync::Mutex<Option<(String, IngestFailurePoint)>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_ingest_step_for_test(
    operation_id: impl Into<String>,
    point: IngestFailurePoint,
) {
    *FAIL_NEXT_INGEST_STEP
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((operation_id.into(), point));
}

#[cfg(test)]
fn maybe_fail_ingest_step_for_test(
    operation_id: &str,
    point: IngestFailurePoint,
) -> Result<(), ApplyOperationError> {
    let mut pending = FAIL_NEXT_INGEST_STEP
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
            "injected ingest failure after {point:?} for {operation_id}"
        )));
    }
    Ok(())
}

pub(crate) async fn ingest_markdown_original(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> Result<Value, String> {
    ingest_markdown_original_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

pub(crate) async fn ingest_markdown_original_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let payload = obj(&operation.payload);
    let pending_original_path = js_string_value(coalesce(
        &payload,
        &["pendingOriginalPath", "pending_original_path"],
    ));
    match crate::operation_completion_ledger::completion_entry_for(app, &operation.operation_id) {
        Ok(Some(entry)) if entry.kind == "document.ingestMarkdownOriginal" => {
            if let Some(pending_path) = pending_original_path.as_deref() {
                cleanup_ingest_pending_best_effort(app, &operation.graph_id, pending_path);
            }
            let mut cached = entry
                .result
                .and_then(|result| result.as_object().cloned())
                .unwrap_or_default();
            cached.insert("replayed".to_string(), json!(true));
            return Ok(Value::Object(cached));
        }
        Ok(Some(entry)) => {
            return Err(ApplyOperationError::terminal(format!(
                "completion ledger operation {} belongs to {}, not document.ingestMarkdownOriginal",
                operation.operation_id, entry.kind
            )));
        }
        Ok(None) => {}
        Err(error) => {
            return Err(ApplyOperationError::retryable_after_hot_commit(format!(
                "read ingest completion ledger: {error}"
            )));
        }
    }

    let result = ingest_markdown_original_steps_classified(app, operation).await?;
    let entry = crate::operation_completion_ledger::OperationCompletionEntry {
        schema_version: 1,
        operation_id: operation.operation_id.clone(),
        kind: "document.ingestMarkdownOriginal".to_string(),
        graph_id: Some(operation.graph_id.clone()),
        completed_at: operation.enqueue_timestamp.clone(),
        payload_hash: None,
        result: Some(result.clone()),
    };
    crate::operation_completion_ledger::append_completion_entry(app, entry)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    if let Some(pending_path) = pending_original_path.as_deref() {
        cleanup_ingest_pending_best_effort(app, &operation.graph_id, pending_path);
    }
    Ok(result)
}

fn cleanup_ingest_pending_best_effort(app: &AppHandle, graph_id: &str, pending_path: &str) {
    if let Err(error) = crate::pending_upload_service::cleanup_pending_upload(
        app.clone(),
        crate::pending_upload_service::PendingUploadFileInput {
            graph_id: Some(graph_id.to_string()),
            pending_path: pending_path.to_string(),
        },
    ) {
        log::warn!("[document.ingestMarkdownOriginal] pending cleanup failed: {error}");
    }
}

pub(crate) async fn ingest_markdown_original_steps_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let graph_id = operation.graph_id.clone();
    let payload = obj(&operation.payload);
    let document_id = js_string_value(coalesce(
        &payload,
        &["documentId", "document_id", "docId", "doc_id"],
    ))
    .ok_or_else(|| {
        "document.ingestMarkdownOriginal: documentId is required (provide via payload or rely on enqueue normalization)"
            .to_string()
    })?;
    let filename = js_string_value(coalesce(
        &payload,
        &["filename", "originalFilename", "original_filename"],
    ))
    .unwrap_or_else(|| "document.pdf".to_string());
    let markdown = js_string_value(coalesce(&payload, &["markdown", "content"]))
        .ok_or_else(|| "document.ingestMarkdownOriginal: markdown is required".to_string())?;

    let parent_id = js_string_value(coalesce(&payload, &["parentId", "parent_id"]));
    let title_override = js_string_value(coalesce(
        &payload,
        &["title", "titleOverride", "title_override"],
    ));
    let declared_mime_type = js_string_value(coalesce(
        &payload,
        &["mimeType", "mime_type", "contentType", "content_type"],
    ))
    .unwrap_or_else(|| "application/pdf".to_string());
    let pending_original_path = js_string_value(coalesce(
        &payload,
        &["pendingOriginalPath", "pending_original_path"],
    ));
    let data_base64 = js_string_value(coalesce(&payload, &["dataBase64", "data_base64"]));
    if pending_original_path.is_none() && data_base64.is_none() {
        return Err(ApplyOperationError::terminal(
            "document.ingestMarkdownOriginal: pendingOriginalPath or dataBase64 is required"
                .to_string(),
        ));
    }
    let requested_approach_id = js_string_value(coalesce(
        &payload,
        &["requestedApproachId", "requested_approach_id"],
    ))
    .unwrap_or_else(|| "pdf.docling-accurate".to_string());
    let ingestion_approach_id = js_string_value(coalesce(
        &payload,
        &["ingestionApproachId", "ingestion_approach_id"],
    ))
    .unwrap_or_else(|| "pdf.docling-accurate".to_string());
    let local_fallback = js_boolean(
        coalesce(&payload, &["localFallback", "local_fallback"]),
        false,
    );
    let ocr_available = js_boolean(
        coalesce(&payload, &["ocrAvailable", "ocr_available"]),
        ingestion_approach_id == "pdf.docling-accurate",
    );

    let parsed = super::content_parse::parse_write_content_for_operation(
        &markdown,
        Some("markdown"),
        &operation.operation_id,
    )?;
    let document_title = {
        let raw = title_override
            .or_else(|| parsed.derived_title.clone())
            .unwrap_or_else(|| filename_stem_title(&filename));
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            "Untitled".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let document_title = crate::ids::normalize_title(&document_title)?;

    let provided_source_file = coalesce(&payload, &["sourceFile", "source_file"])
        .map(obj)
        .unwrap_or_default();
    let storage_key = js_string_value(coalesce(
        &provided_source_file,
        &["storageKey", "sf_storageKey"],
    ))
    .unwrap_or_else(|| {
        format!(
            "local://documents/{document_id}/original/{}",
            encode_uri_component(&filename)
        )
    });
    let original_filename = js_string_value(coalesce(
        &provided_source_file,
        &[
            "originalFilename",
            "original_filename",
            "sf_originalFilename",
        ],
    ))
    .unwrap_or_else(|| filename.clone());
    let mime_type = js_string_value(coalesce(
        &provided_source_file,
        &["mimeType", "mime_type", "sf_mimeType"],
    ))
    .unwrap_or_else(|| declared_mime_type.clone());
    let size_bytes = js_finite_number(
        coalesce(
            &provided_source_file,
            &["sizeBytes", "size_bytes", "sf_sizeBytes"],
        )
        .or_else(|| coalesce(&payload, &["sizeBytes", "size_bytes"])),
    )
    .unwrap_or(0.0);
    let file_type = js_string_value(coalesce(
        &provided_source_file,
        &["fileType", "file_type", "sf_fileType"],
    ))
    .unwrap_or_else(|| "pdf".to_string());

    // Finish permanent input/path validation before the first durable step.
    let graph_dir = existing_graph_dir(app, &graph_id)?;
    let state_path = checked_document_ydoc_state_path(&graph_dir, &document_id)?;
    if let Some(pending_path) = pending_original_path.as_deref() {
        crate::pending_upload_service::resolved_pending_upload_path(
            app,
            Some(&graph_id),
            pending_path,
        )?;
    } else if let Some(encoded) = data_base64.as_deref() {
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|error| format!("decode original file: {error}"))?;
    }

    // createDocument (idempotent: an existing manifest returns the record).
    let create_input: crate::document_types::CreateDocumentInput = serde_json::from_value(json!({
        "graphId": graph_id,
        "title": document_title,
        "documentId": document_id,
    }))
    .map_err(|error| format!("assemble create_document input: {error}"))?;
    crate::document_service::create_document_with_lease(app.clone(), create_input)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;

    // Original file: adopt a staged upload, or write the manifest from base64.
    if let Some(pending_path) = &pending_original_path {
        let input: crate::original_file_types::AdoptPendingOriginalFileInput =
            serde_json::from_value(json!({
                    "graphId": graph_id,
                    "documentId": document_id,
                    "pendingPath": pending_path,
                    "filename": original_filename,
                    "mimeType": mime_type,
            }))
            .map_err(|error| {
                ApplyOperationError::retryable_after_hot_commit(format!(
                    "assemble adopt original input: {error}"
                ))
            })?;
        crate::original_file_service::copy_pending_original_file_service(app, input)
            .map_err(|error| ApplyOperationError::retryable_after_hot_commit(error.message()))?;
    } else {
        let input: crate::original_file_types::SaveOriginalFileInput =
            serde_json::from_value(json!({
                    "graphId": graph_id,
                    "documentId": document_id,
                    "filename": original_filename,
                    "mimeType": mime_type,
                    "dataBase64": data_base64.clone().unwrap_or_default(),
            }))
            .map_err(|error| {
                ApplyOperationError::retryable_after_hot_commit(format!(
                    "assemble save original input: {error}"
                ))
            })?;
        crate::original_file_service::save_original_file_manifest_service(app, input)
            .map_err(|error| ApplyOperationError::retryable_after_hot_commit(error.message()))?;
    }
    #[cfg(test)]
    maybe_fail_ingest_step_for_test(&operation.operation_id, IngestFailurePoint::AfterOriginal)?;

    // Workspace entry: readOnly with the full sf_* sourceFile metadata.
    let order = super::workspace_ops::numeric_timestamp(&operation.enqueue_timestamp);
    super::workspace_ops::upsert_document_entry(
        app,
        &graph_id,
        &json!({
            "documentId": document_id,
            "title": document_title,
            "parentId": parent_id,
            "order": order,
            "updatedAt": order,
            "readOnly": true,
            "sourceFile": {
                "storageKey": storage_key,
                "originalFilename": original_filename,
                "mimeType": mime_type,
                "sizeBytes": size_bytes,
                "fileType": file_type,
            },
        }),
        &operation.operation_id,
    )
    .await
    .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    #[cfg(test)]
    maybe_fail_ingest_step_for_test(&operation.operation_id, IngestFailurePoint::AfterWorkspace)?;

    // saveDocumentSnapshot equivalent: rebuild the room fragment from the
    // parsed TipTap JSON (live collaborators see it as a sync update), then
    // materialize + persist via save_document.
    let registry = app.state::<RoomRegistry>();
    let room = registry
        .get_or_create(&format!("doc:{graph_id}:{document_id}"), state_path)
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    let content_nodes: Vec<Value> = parsed
        .tiptap_json
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    room.update_doc(move |_doc, txn| {
        let fragment = txn.get_or_insert_xml_fragment("content");
        let len = fragment.len(txn);
        if len > 0 {
            fragment.remove_range(txn, 0, len);
        }
        builder::append_nodes(txn, &fragment, &content_nodes);
        Ok(())
    })
    .await
    .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    #[cfg(test)]
    maybe_fail_ingest_step_for_test(&operation.operation_id, IngestFailurePoint::AfterHot)?;
    let record_value = persist_room_document(
        app,
        &graph_id,
        &document_id,
        &document_title,
        &room,
        &operation.operation_id,
    )
    .await
    .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    #[cfg(test)]
    maybe_fail_ingest_step_for_test(&operation.operation_id, IngestFailurePoint::AfterTail)?;

    let block_count = record_value
        .get("blocks")
        .and_then(Value::as_array)
        .map(|blocks| blocks.len())
        .unwrap_or(0);
    let mut stats = payload.get("stats").map(obj).unwrap_or_default();
    stats.insert("blockCount".to_string(), json!(block_count));
    stats.insert("block_count".to_string(), json!(block_count));
    let mut warnings = js_string_array(payload.get("warnings"));
    warnings.extend(parsed.warnings.clone());

    Ok(json!({
        "documentId": document_id,
        "title": document_title,
        "fileType": "pdf",
        "readOnly": true,
        "requestedApproachId": requested_approach_id,
        "ingestionApproachId": ingestion_approach_id,
        "localFallback": local_fallback,
        "ocrAvailable": ocr_available,
        "warnings": warnings,
        "stats": stats,
        "sourceFile": {
            "storageKey": storage_key,
            "originalFilename": original_filename,
            "mimeType": mime_type,
            "sizeBytes": json_number(size_bytes),
            "fileType": file_type,
        },
    }))
}
