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
use super::minimal_edit::{
    self, CurrentFragment, DesiredFragment, EmitStats, IdPolicy, LiveEdits, PlanSummary,
    SourceFormat, WritePolicy,
};

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
///
/// "Already applied" is asked of the planner: the hot Y.Doc equals the request
/// when planning the request against it is a no-op (and the supplied comments
/// are stored), and the cold record equals the request when it carries the
/// title and the very projection the hot Doc has. The two are read in sequence
/// under the Doc lock, never in nested transactions.
async fn preflight_document_write_revision(
    title: &str,
    desired: &DesiredFragment,
    policy: &WritePolicy,
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
    let (hot_matches, live_json) = room
        .with_doc(|doc| {
            let txn = doc.transact();
            let current = match txn.get_xml_fragment("content") {
                Some(fragment) => minimal_edit::model::read_fragment(&txn, &fragment, policy),
                None => CurrentFragment::default(),
            };
            let desired = keep_trailing_empty_paragraph(&current, desired.clone(), policy);
            let noop = minimal_edit::plan::plan_fragment(&current, &desired, policy).is_noop();
            let live_json = projection::tiptap_json_in(&txn);
            drop(txn);
            (noop && supplied_comments_match(doc, comments), live_json)
        })
        .await;
    // The record is compared with the hot projection in the canonical mark
    // order (`tiptap_json_matches`): a run's marks are a set, and a record saved
    // by an older build may hold any order.
    let cold_matches =
        record.title == title && tiptap_json_matches(record.tiptap_json.as_ref(), Some(&live_json));
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

    // Placement is the caller's to change, not a side effect of writing
    // content. A payload that names `parentId` (a folder, or an explicit null
    // for the root) or `order` moves the document; one that names neither —
    // every MCP `write_document`, Hoja, Choreograph — leaves an existing
    // workspace entry where it is. The fallbacks below only file a document
    // the write is creating. (Hosted Hoja QA 2026-09-23: every content write
    // refiled the document at the root with a fresh order.)
    let parent_named = payload.contains_key("parentId") || payload.contains_key("parent_id");
    let parent_id = str_field(&payload, "parentId").or_else(|| str_field(&payload, "parent_id"));
    let explicit_order = num_field(&payload, "order");
    let order = explicit_order
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
    // Receipt paths name the caller's own carrier: `content[i]` for a
    // TipTap document, `blocks[i]` for a blocks array.
    let mut receipt_prefix = "content";
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
        receipt_prefix = "blocks";
        let (tiptap_json, notes) = tiptap_json_from_blocks_array(payload.get("blocks"));
        warnings.extend(notes);
        tiptap_json
    };
    // The block contract: every JSON write path stores canonical nodes.
    // Known-equivalent shapes are rewritten deterministically, unknown nodes
    // are wrapped byte-exact as opaqueBlock, and every rewrite is a receipt.
    let tiptap_json = {
        let nodes = tiptap_json
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let (nodes, receipts) = super::block_contract::normalise_doc_content_at(
            nodes,
            &operation.operation_id,
            receipt_prefix,
        );
        warnings.extend(receipts);
        json!({"type": "doc", "content": nodes})
    };

    let graph_dir = existing_graph_dir(app, &graph_id)?;
    // Every failure of the recreation step below is retried, and it refuses an
    // id that is not a local id: such a write parked, and every later write to
    // the graph behind it, forever (2026-10-14). Refuse it here.
    crate::ids::validate_local_id(&document_id, "document_id")?;
    // A write never brings a deleted document back (2026-10-14; W6 probe F3:
    // a write to a tombstoned id resurrected it with the caller's content).
    // Recreation is a trusted internal act, marked by the server only
    // (`crdt_queue::TRUSTED_RECREATION_PAYLOAD_KEY`, stripped from every
    // caller payload). A tombstone read that fails is not a refusal: the
    // recreation step below reads it again, and its failure is retried.
    let trusted_recreation = payload
        .get(crate::crdt_queue::TRUSTED_RECREATION_PAYLOAD_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !trusted_recreation
        && matches!(
            crate::document_tombstone_store::document_is_tombstoned(&graph_dir, &document_id),
            Ok(true)
        )
    {
        return Err(ApplyOperationError::terminal(format!(
            "write_document: the document {document_id:?} was deleted, and a write does not bring \
             a deleted document back. Bringing it back is an explicit act: source_push \
             documentLifecycle \"recreate\" with its prior documentIncarnation (for a deletion the \
             source ledger recorded); its retained history stays readable (get_document_history, \
             read_document_at_snapshot). To write new content, create a new document \
             (create_document, or write_document with an unused id)."
        )));
    }
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

    // A scene kind (the Flow board, the whiteboard) is not a TipTap fragment:
    // a content write would add a `content` root to a board that has none and
    // show nothing. Refuse it and name the tool that does write the board.
    let document_kind = resolve_document_kind(app, &graph_id, &document_id).await?;
    if crate::whiteboard::is_scene_kind(document_kind.as_deref()) {
        let (kind, tool) = if crate::whiteboard::is_whiteboard(document_kind.as_deref()) {
            ("whiteboard", "whiteboard_apply")
        } else {
            ("Flow board", "flow_seed_board")
        };
        return Err(ApplyOperationError::terminal(format!(
            "write_document: the document {document_id:?} is a {kind}, not a text document, so \
             content cannot be written to it. Use {tool} to change it."
        )));
    }

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
    // The carrier decides what the write states (DESIGN §2.3): a `tiptapJson`
    // payload owns every attribute and mark, a `content` string only what its
    // format can say, a `blocks` array only the simplified block's fields.
    let format = if payload.get("tiptapJson").is_some_and(Value::is_object) {
        SourceFormat::TiptapJson
    } else if from_parsed_content {
        SourceFormat::from_parsed(source_format.as_deref().unwrap_or(""))
    } else {
        SourceFormat::Blocks
    };
    // Ids the content parser minted for this operation mean nothing and a
    // matched block keeps its current id. Markdown and plain text carry no ids,
    // so every id in the parse was minted; XML and HTML may carry the caller's
    // own, which are the ids that occur in the content string.
    let ids = if from_parsed_content {
        let mut minted = std::collections::BTreeSet::new();
        collect_block_ids(&content_nodes, &mut minted);
        if matches!(format, SourceFormat::Xml | SourceFormat::Html) {
            let content = payload.get("content").and_then(Value::as_str).unwrap_or("");
            minted.retain(|id| !content.contains(id.as_str()));
        }
        IdPolicy::Adopt { minted }
    } else {
        // tiptapJson and blocks have been through the block-contract normaliser,
        // which mints ids for the nodes it rewrites or hoists (flattened list
        // items, paragraphs split around an image, items without ids). Those are
        // not the caller's: only ids the caller sent are bound, or the same write
        // twice would re-id those blocks every time.
        let mut minted = std::collections::BTreeSet::new();
        collect_block_ids(&content_nodes, &mut minted);
        let mut sent = std::collections::BTreeSet::new();
        if let Some(raw) = payload.get("tiptapJson") {
            collect_id_strings(raw, &mut sent);
        }
        if let Some(raw) = payload.get("blocks") {
            collect_id_strings(raw, &mut sent);
        }
        minted.retain(|id| !sent.contains(id));
        IdPolicy::Adopt { minted }
    };
    let policy = WritePolicy { format, ids };
    let desired = minimal_edit::model::desired_from_tiptap_nodes(&content_nodes, &policy);

    let expected_revision = payload.get("expectedRevision");
    let revision_disposition = preflight_document_write_revision(
        &title,
        &desired,
        &policy,
        &comments,
        existing_record.as_ref(),
        &room,
        expected_revision,
    )
    .await?;

    // What the agent read: the record at the revision it names. Only then is
    // the record a base for the three-way rebase of a live edit it never saw.
    let rebase_base = if minimal_edit::REBASE_ON_LIVE_EDITS {
        match (
            expected_revision.and_then(Value::as_u64),
            existing_record.as_ref(),
        ) {
            (Some(expected), Some(record)) if record.revision == expected => record
                .tiptap_json
                .as_ref()
                .and_then(|json| json.get("content"))
                .and_then(Value::as_array)
                .map(|nodes| minimal_edit::model::desired_from_tiptap_nodes(nodes, &policy)),
            // Vera 10-21: a person's edit wins over every agent write, not only one that names its
            // revision. Without expectedRevision the saved record stands in for what the agent read
            // (it is what read_document serves), so an edit the room holds and the record does not yet
            // (typed after the read, before this write) is kept, or reported as a conflict.
            (None, Some(record)) => record
                .tiptap_json
                .as_ref()
                .and_then(|json| json.get("content"))
                .and_then(Value::as_array)
                .map(|nodes| minimal_edit::model::desired_from_tiptap_nodes(nodes, &policy)),
            _ => None,
        }
    } else {
        None
    };

    let mut write_outcome: Option<DocumentWriteOutcome> = None;
    if revision_disposition == DocumentWriteRevisionDisposition::Apply {
        let outcome = room
            .update_doc(|_doc, txn| {
                // doc.get_or_insert_* opens its own transaction and panics while
                // one is live (yrs 0.27): use the in-transaction variants.
                let fragment = txn.get_or_insert_xml_fragment("content");
                let read_started = std::time::Instant::now();
                let current = minimal_edit::model::read_fragment(&*txn, &fragment, &policy);
                let read_ms = elapsed_ms(read_started);

                let plan_started = std::time::Instant::now();
                let (desired, live_edits) = match rebase_base.as_ref() {
                    Some(base) => {
                        minimal_edit::rebase::rebase_on_live(base, &current, desired, &policy)
                    }
                    None => (desired, LiveEdits::default()),
                };
                // After the rebase: an unsaved trailing paragraph is already kept there as a live edit, and adding
                // one before it would make two.
                let desired = keep_trailing_empty_paragraph(&current, desired, &policy);
                let plan = minimal_edit::plan::plan_fragment(&current, &desired, &policy);
                let plan_ms = elapsed_ms(plan_started);

                let emit_started = std::time::Instant::now();
                let stats = minimal_edit::emit::emit_fragment(txn, &fragment, &current, &plan)?;
                let mut comments_written = 0u32;
                if !comments.is_empty() {
                    let comments_map = txn.get_or_insert_map("comments");
                    // Signed by the server, as edit_comment's are (2026-10-14).
                    let author = crate::comment_authorship::CommentAuthor::from_payload(&payload);
                    for (comment_id, data) in &comments {
                        let mut data = data.clone();
                        let stored = match comments_map.get(&*txn, comment_id.as_str()) {
                            Some(Out::Any(any)) => Some(any_json(&any)),
                            _ => None,
                        };
                        if let Value::Object(object) = &mut data {
                            let claimed = object.get("author").cloned();
                            crate::comment_authorship::sign_comment(
                                object,
                                &author,
                                claimed.as_ref(),
                                stored.as_ref().and_then(Value::as_object),
                            );
                        }
                        // An unchanged comment is not written again: a write
                        // that sends back what is stored changes nothing.
                        if stored
                            .as_ref()
                            .is_some_and(|stored| json_semantically_equal(stored, &data))
                        {
                            continue;
                        }
                        comments_map.insert(txn, comment_id.as_str(), builder::json_to_any(&data));
                        comments_written += 1;
                    }
                }
                let emit_ms = elapsed_ms(emit_started);
                Ok(DocumentWriteOutcome {
                    stats,
                    summary: plan.summary,
                    live_edits,
                    comments_written,
                    read_ms,
                    plan_ms,
                    emit_ms,
                })
            })
            .await
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        write_outcome = Some(outcome);
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
    //
    // A write that changed nothing persists nothing: zero Yjs operations, no
    // comment written, no projection owed, the record already carries this title.
    // With `expectedRevision` the write persists as always, because the contract
    // bumps the revision of an equal write at `expected`.
    let no_op_write = expected_revision.map_or(true, Value::is_null)
        && recreation.is_none()
        && write_outcome
            .as_ref()
            .is_some_and(|outcome| outcome.stats.counts.is_zero() && outcome.comments_written == 0)
        && !room.needs_projection_flush()
        && existing_record
            .as_ref()
            .is_some_and(|record| record.title == title);
    let record_result = match (no_op_write, existing_record.as_ref()) {
        (true, Some(record)) => serde_json::to_value(record)
            .map_err(|error| format!("serialize unchanged document record: {error}")),
        _ => {
            persist_room_document_with_expected_revision(
                app,
                &graph_id,
                &document_id,
                &title,
                &room,
                &operation.operation_id,
                expected_revision,
                recreation
                    .as_ref()
                    .map(|tombstone| tombstone.deletion_id.as_str()),
            )
            .await
        }
    };
    let persisted = !no_op_write;
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
            "preserveParent": !parent_named,
            "preserveOrder": explicit_order.is_none(),
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
        "parentId": workspace.get("parentId").cloned().unwrap_or(json!(parent_id)),
        "workspace": workspace,
        "blockCount": blocks.len(),
        "rdfTripleCount": record_value.get("rdfTripleCount").cloned().unwrap_or(json!(0)),
        "blockIds": blocks
            .iter()
            .filter_map(|b| b.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        "sourceFormat": source_format,
        "warnings": warnings,
        "crdt": crdt_report(write_outcome.as_ref(), persisted),
    }))
}

/// What one content write did to the Y.Doc, measured in the transaction that did it.
struct DocumentWriteOutcome {
    stats: EmitStats,
    summary: PlanSummary,
    live_edits: LiveEdits,
    comments_written: u32,
    read_ms: f64,
    plan_ms: f64,
    emit_ms: f64,
}

fn elapsed_ms(started: std::time::Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// The `crdt` field of a write's result: the counted Yjs operations, the plan's shape, the live edits a
/// rebase kept, the writer's client id and clock range (the attribution seam), timings, and whether the
/// write persisted. A replay (already applied, nothing run) reports zero operations.
fn crdt_report(outcome: Option<&DocumentWriteOutcome>, persisted: bool) -> Value {
    fn to_json<T: serde::Serialize>(value: &T) -> Value {
        serde_json::to_value(value).unwrap_or(Value::Null)
    }
    match outcome {
        Some(outcome) => json!({
            "ops": to_json(&outcome.stats.counts),
            "plan": to_json(&outcome.summary),
            "liveEdits": to_json(&outcome.live_edits),
            "clientId": outcome.stats.client_id,
            "clocks": [outcome.stats.clock_before, outcome.stats.clock_after],
            "commentsWritten": outcome.comments_written,
            "ms": {"read": outcome.read_ms, "plan": outcome.plan_ms, "emit": outcome.emit_ms},
            "persisted": persisted,
        }),
        None => json!({
            "ops": to_json(&minimal_edit::OpCounts::default()),
            "plan": to_json(&PlanSummary::default()),
            "liveEdits": to_json(&LiveEdits::default()),
            "replayed": true,
            "persisted": persisted,
        }),
    }
}

/// Every block id (`data-block-id` or `blockId`) in a TipTap node list, at any depth.
fn collect_block_ids(nodes: &[Value], out: &mut std::collections::BTreeSet<String>) {
    for node in nodes {
        if let Some(attrs) = node.get("attrs").and_then(Value::as_object) {
            for key in ["data-block-id", "blockId"] {
                if let Some(id) = attrs.get(key).and_then(Value::as_str) {
                    if !id.is_empty() {
                        out.insert(id.to_string());
                    }
                }
            }
        }
        if let Some(children) = node.get("content").and_then(Value::as_array) {
            collect_block_ids(children, out);
        }
    }
}

/// The editor keeps an empty paragraph at the end of every document (its trailing-node plugin), and markdown
/// cannot say an empty paragraph (the exporter skips it). A write that ends without one would remove it and the
/// editor would add it straight back: a remove, a re-add, a broadcast and a new revision on every agent
/// write-back to an open document (live check L5, 10-21). So when the document ends with an empty paragraph
/// and the write does not, the write keeps it, after everything it states. Restore states everything exactly.
fn keep_trailing_empty_paragraph(
    current: &CurrentFragment,
    mut desired: DesiredFragment,
    policy: &WritePolicy,
) -> DesiredFragment {
    if policy.format == SourceFormat::Restore {
        return desired;
    }
    let is_empty_paragraph = |child: &minimal_edit::CurrentChild| {
        match child {
        minimal_edit::CurrentChild::Element(node) => {
            node.tag == "paragraph"
                && node.children.iter().all(|inner| {
                    matches!(inner, minimal_edit::CurrentChild::Text(text) if text.byte_len == 0)
                })
        }
        minimal_edit::CurrentChild::Text(_) => false,
    }
    };
    let desired_ends_empty = matches!(
        desired.children.last(),
        Some(minimal_edit::DesiredChild::Element(node))
            if node.tag == "paragraph" && node.children.is_empty()
    );
    if current.children.last().is_some_and(is_empty_paragraph) && !desired_ends_empty {
        let trailing =
            minimal_edit::model::desired_from_tiptap_nodes(&[json!({"type": "paragraph"})], policy);
        desired.children.extend(trailing.children);
    }
    desired
}

/// Every string under an id key (`id`, `blockId`, `data-block-id`) anywhere in a caller's payload value: the
/// ids the caller sent, whatever the carrier's shape.
fn collect_id_strings(value: &Value, out: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if matches!(key.as_str(), "id" | "blockId" | "data-block-id") {
                    if let Some(id) = child.as_str().filter(|id| !id.is_empty()) {
                        out.insert(id.to_string());
                    }
                }
                collect_id_strings(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_id_strings(item, out)),
        _ => {}
    }
}

/// JSON equality that reads a whole number the same however it was stored: the Y.Doc keeps every number
/// as an f64, a caller's JSON may say `1`.
fn json_semantically_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| json_semantically_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| json_semantically_equal(a, b)))
        }
        _ => left == right,
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

/// Block types the simplified `blocks` array maps (blockToTipTapNode).
const SIMPLIFIED_BLOCK_TYPES: &[&str] = &[
    "paragraph",
    "heading",
    "bullet",
    "numbered",
    "todo",
    "quote",
    "code",
    "divider",
];

/// A `blocks` entry that is a TipTap node rather than a simplified block:
/// its `content` is an array, or it carries `attrs`. A simplified block
/// (`{id, type, content: string, marks}`) has neither. Before the block
/// contract a TipTap node here lost its content (it became an empty
/// paragraph); now it goes through the contract normaliser like any other
/// TipTap JSON.
pub(crate) fn is_tiptap_shaped_block(value: &Value) -> bool {
    value.get("content").is_some_and(Value::is_array)
        || value.get("attrs").is_some_and(Value::is_object)
}

/// The nodes of a `blocks` array: TipTap nodes verbatim (the contract
/// normaliser runs next), simplified blocks through blockToTipTapNode. A
/// simplified type outside the blocks vocabulary keeps the historical
/// paragraph fallback, now with a receipt.
pub(crate) fn nodes_from_blocks_array(
    blocks: &[Value],
) -> Result<(Vec<Value>, Vec<String>), String> {
    let mut nodes = Vec::with_capacity(blocks.len());
    let mut warnings = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if is_tiptap_shaped_block(block) {
            nodes.push(block.clone());
            continue;
        }
        if let Some(block_type) = block.get("type").and_then(Value::as_str) {
            if !SIMPLIFIED_BLOCK_TYPES.contains(&block_type) {
                warnings.push(format!(
                    "block-contract fallback blocks[{index}]: block type {block_type} is not a blocks-array type; stored as a paragraph (text kept)"
                ));
            }
        }
        nodes.push(block_to_tiptap_node(block)?);
    }
    Ok((nodes, warnings))
}

/// `tiptap_json_from_blocks` for a write: TipTap-shaped entries kept for the
/// normaliser, plus the fallback receipts.
fn tiptap_json_from_blocks_array(value: Option<&Value>) -> (Value, Vec<String>) {
    let blocks = value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if blocks.is_empty() {
        return (
            json!({ "type": "doc", "content": [{ "type": "paragraph" }] }),
            Vec::new(),
        );
    }
    match nodes_from_blocks_array(blocks) {
        Ok((nodes, warnings)) => (json!({ "type": "doc", "content": nodes }), warnings),
        Err(_) => (json!({ "type": "doc", "content": [] }), Vec::new()),
    }
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
    let document_kind = resolve_document_kind(app, graph_id, document_id).await?;
    let materialized = materialize_room_document(
        graph_id,
        document_id,
        title,
        room,
        operation_id,
        None,
        document_kind.as_deref(),
    )
    .await?;
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
    // Flow boards project into the graph's `:projection:flow` lane instead of
    // a TipTap tree; the replay reconciles it from the same verified record
    // (no-op for every other kind). Unit G3.
    crate::flow_board_reconcile::reconcile_flow_board_record(&store, &graph_dir, &record)?;
    crate::pdf_source::reconcile(&store, &record)?;
    // Whiteboards: the `:projection:whiteboard` lane, from the same verified record (no-op for every other kind).
    crate::whiteboard::lane::reconcile_record(&store, &graph_dir, &record)?;
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

/// Resolve the workspace-declared `documentKind` for one document (unit G3):
/// the live workspace room when hosted (`RoomRegistry::peek` — never
/// `get_or_create`, a document flush must not host/hydrate the workspace room
/// as a side effect), else the durable workspace Y.Doc sidecar (authoritative
/// at rest — every accepted workspace mutation persists it before broadcast).
/// A graph with no workspace sidecar yet has declared no kinds ⇒ `None`.
/// FAIL CLOSED on a corrupt sidecar: refusing to guess a kind beats silently
/// projecting a board through the TipTap seam (or vice versa).
pub(crate) async fn resolve_document_kind(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Option<String>, String> {
    let registry = app.state::<super::rooms::RoomRegistry>();
    if let Some(room) = registry.peek(&format!("workspace:{graph_id}")).await {
        let document_id = document_id.to_string();
        return Ok(room
            .with_doc(move |doc| super::workspace_ops::document_kind(doc, &document_id))
            .await);
    }
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let sidecar = crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir);
    if !sidecar.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&sidecar)
        .map_err(|error| format!("read workspace Y.Doc sidecar for kind resolution: {error}"))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let update = Update::decode_v1(&bytes)
        .map_err(|error| format!("decode workspace Y.Doc sidecar for kind resolution: {error}"))?;
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(update).map_err(|error| {
            format!("apply workspace Y.Doc sidecar for kind resolution: {error}")
        })?;
    }
    Ok(super::workspace_ops::document_kind(&doc, document_id))
}

/// Materialize one room Y.Doc into a save candidate.
///
/// `document_kind` branches the ONE TipTap seam (unit G3): for a
/// `"flow-board"` document the TipTap projection is skipped ENTIRELY — no
/// `materialize_ydoc`, no `ydoc_to_tiptap_xml`, and crucially no
/// `get_or_insert_xml_fragment("content")` (which would insert an empty
/// `content` root into a board doc whose only roots are `resource`/`scene`).
/// The candidate's content fields are empty BY CONSTRUCTION (`body:""`,
/// `tiptapXml:""`, `tiptapJson:null`, `tree:null`, `blocks:[]`), the Y.Doc
/// bytes are saved verbatim exactly as for TipTap documents, and the record
/// carries `documentKind:"flow-board"` so every cold path (persistence tail,
/// history snapshots, restore) can take the same branch. The graph's
/// `:projection:flow` lane is reconciled from those bytes downstream, in the
/// shared persistence tail (`flow_board_reconcile::reconcile_flow_board_record`).
#[tracing::instrument(target = "gardend_otel", name = "document.materialize", skip_all,
    fields(sophia.graph.id = graph_id, sophia.document.id = document_id,
        sophia.document.blocks = tracing::field::Empty,
        sophia.document.body_bytes = tracing::field::Empty,
        sophia.ydoc.state_bytes = tracing::field::Empty))]
pub(super) async fn materialize_room_document(
    graph_id: &str,
    document_id: &str,
    title: &str,
    room: &super::rooms::Room,
    operation_id: &str,
    expected_revision: Option<&Value>,
    document_kind: Option<&str>,
) -> Result<MaterializedRoomDocument, String> {
    if crate::whiteboard::is_scene_kind(document_kind) {
        let is_whiteboard = crate::whiteboard::is_whiteboard(document_kind);
        let (projection_epoch, (ydoc_state, body, blocks)) = room
            .with_doc_version(|doc| {
                // A whiteboard's saved `body` is its outline markdown, so search,
                // `read_document` and the semantic index see the board's words,
                // and its `blocks` list one block per live shape (the element
                // id), read from the same scene in the same closure; a flow
                // board's content fields stay empty.
                let (body, blocks) = if is_whiteboard {
                    let scene = crate::whiteboard::ops::scene_of_doc(doc);
                    (
                        crate::whiteboard::outline::outline_markdown(title, &scene),
                        crate::whiteboard::blocks::blocks_of(&scene),
                    )
                } else {
                    (String::new(), Vec::new())
                };
                let txn = doc.transact();
                (
                    txn.encode_state_as_update_v1(&yrs::StateVector::default()),
                    body,
                    blocks,
                )
            })
            .await;
        tracing::Span::current().record("sophia.document.blocks", blocks.len() as u64);
        tracing::Span::current().record("sophia.document.body_bytes", body.len() as u64);
        tracing::Span::current().record("sophia.ydoc.state_bytes", ydoc_state.len() as u64);
        let blocks_json = serde_json::to_value(&blocks)
            .map_err(|error| format!("serialize whiteboard blocks: {error}"))?;
        let ydoc_b64 = base64::engine::general_purpose::STANDARD.encode(ydoc_state);
        let candidate = json!({
            "graphId": graph_id,
            "documentId": document_id,
            "title": title,
            "body": body,
            "tiptapXml": "",
            "tiptapJson": Value::Null,
            "ydocUpdateBase64": ydoc_b64,
            "tree": Value::Null,
            "blocks": blocks_json,
            "documentKind": document_kind,
            "traceOperationId": operation_id,
            "expectedRevision": expected_revision.cloned(),
        });
        let save_input: SaveDocumentInput = serde_json::from_value(candidate.clone())
            .map_err(|error| format!("assemble flow-board save_document input: {error}"))?;
        return Ok(MaterializedRoomDocument {
            projection_epoch,
            candidate,
            save_input,
        });
    }

    let (projection_epoch, (snapshot, tiptap_xml, ydoc_state)) = room
        .with_doc_version(|doc| {
            let snapshot = projection::materialize_ydoc(doc, document_id);
            let tiptap_xml = projection::ydoc_to_tiptap_xml(doc);
            let txn = doc.transact();
            let ydoc_state = txn.encode_state_as_update_v1(&yrs::StateVector::default());
            (snapshot, tiptap_xml, ydoc_state)
        })
        .await;
    tracing::Span::current().record(
        "sophia.document.blocks",
        snapshot.blocks_json.as_array().map_or(0, Vec::len) as u64,
    );
    tracing::Span::current().record("sophia.document.body_bytes", snapshot.body.len() as u64);
    tracing::Span::current().record("sophia.ydoc.state_bytes", ydoc_state.len() as u64);
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
    // A text run's marks are a set, and the ORDER stored in a record is not
    // part of the document: yrs returned a run's formatting in HashMap order
    // until `marks_from_attrs` began sorting its reads with the contract's
    // `canonical_mark_cmp`, so a record saved before that holds any order while
    // a fresh projection of its own Y.Doc holds the canonical one. `tiptapJson`
    // is therefore compared with both sides in the canonical mark order; title,
    // body and documentKind stay exact.
    let content_matches = ["title", "body", "documentKind"]
        .into_iter()
        .all(|field| record.get(field) == candidate.get(field))
        && tiptap_json_matches(record.get("tiptapJson"), candidate.get("tiptapJson"));
    if !content_matches {
        return Ok(false);
    }
    // A flow board's semantics live ENTIRELY outside the TipTap fields (its
    // title/body/tiptapJson are empty by construction), so the fields above
    // would call EVERY board edit "unchanged" and route it through the
    // repair-replay path — stale record bytes, no revision, a stale
    // `:projection:flow` reconcile. Compare the board's canonical projection
    // instead: the deterministic form-C export of each side's Y.Doc state
    // (raw update bytes stay a non-key here too — equal boards with different
    // Yjs histories still compare equal). Unit G3.
    let record_kind = record.get("documentKind").and_then(Value::as_str);
    if record_kind == Some(crate::flow_board::FLOW_BOARD_KIND) {
        if canonical_board_projection(record)? != canonical_board_projection(candidate)? {
            return Ok(false);
        }
    }
    // A whiteboard's outline (its `body`, compared above) leaves out colours,
    // sizes and positions below the reading order, so compare the elements too:
    // a recolour must not read as "unchanged".
    if crate::whiteboard::is_whiteboard(record_kind) {
        let canonical = |value: &Value| -> Result<String, String> {
            let encoded = value
                .get("ydocUpdateBase64")
                .and_then(Value::as_str)
                .unwrap_or("");
            let scene = crate::whiteboard::ops::scene_from_update_base64(encoded)?;
            serde_json::to_string(&scene.elements).map_err(|error| error.to_string())
        };
        if canonical(record)? != canonical(candidate)? {
            return Ok(false);
        }
    }
    Ok(canonical_projection_comments(record)? == canonical_projection_comments(candidate)?)
}

/// One record's `tiptapJson` against a candidate's: equal, or equal once every
/// text run's marks are in the canonical order (`canonical_mark_order`: same
/// marks, same attrs, same everything else). An absent field and a present one
/// still differ, and `null` (a flow board's) is compared as itself.
fn tiptap_json_matches(record: Option<&Value>, candidate: Option<&Value>) -> bool {
    match (record, candidate) {
        (Some(record), Some(candidate)) => {
            record == candidate
                || super::block_contract::canonical_mark_order(record)
                    == super::block_contract::canonical_mark_order(candidate)
        }
        (record, candidate) => record == candidate,
    }
}

/// The value-canonical board content of one save-candidate/record JSON: the
/// deterministic form-C export of its inline Y.Doc state. An empty/absent
/// inline payload canonicalizes to `""` — distinct from every real export
/// (which always starts with `{`), so a restored record whose inline payload
/// was deliberately cleared never reads as equal to a live board.
fn canonical_board_projection(value: &Value) -> Result<String, String> {
    let encoded = value
        .get("ydocUpdateBase64")
        .and_then(Value::as_str)
        .unwrap_or("");
    if encoded.is_empty() {
        return Ok(String::new());
    }
    let doc = crate::flow_board::board_doc_from_update_base64(encoded)?;
    Ok(crate::flow_board::board_export_json(&doc, None).json)
}

/// `projection_semantics_match` and the order of a text run's marks: a record
/// saved before the projection sorted its reads may hold the marks in any order,
/// and must still be recognised as the document its own Y.Doc projects. The
/// negative controls pin that nothing else became loose.
#[cfg(test)]
mod semantics_match_mark_order_tests {
    use super::*;

    fn mark(name: &str) -> Value {
        json!({ "type": name })
    }

    fn link(href: &str) -> Value {
        json!({ "type": "link", "attrs": { "href": href } })
    }

    /// A record (or a save candidate) of one paragraph whose only text run
    /// carries `marks`, in that order. `body` is the same for every caller so a
    /// difference in `tiptapJson` is the only difference between two of these.
    fn paragraph_doc(title: &str, text: &str, marks: &[Value]) -> Value {
        json!({
            "title": title,
            "body": "stacked",
            "tiptapJson": {
                "type": "doc",
                "content": [{
                    "type": "paragraph",
                    "attrs": { "data-block-id": "p" },
                    "content": [{ "type": "text", "text": text, "marks": marks }],
                }],
            },
        })
    }

    fn matches(record: &Value, candidate: &Value) -> bool {
        projection_semantics_match(record, candidate).expect("the two documents are comparable")
    }

    /// The pair from the build box: a record saved by an older build holds
    /// [superscript, bold]; a fresh projection of its own Y.Doc reads
    /// [bold, superscript].
    #[test]
    fn a_record_with_marks_in_an_old_order_matches_the_canonical_projection_of_its_doc() {
        let old = paragraph_doc("Doc", "stacked", &[mark("superscript"), mark("bold")]);
        let fresh = paragraph_doc("Doc", "stacked", &[mark("bold"), mark("superscript")]);
        assert!(
            matches(&old, &fresh),
            "[superscript, bold] against [bold, superscript]"
        );
        assert!(matches(&fresh, &old), "the comparison is symmetric");
        assert!(matches(&old, &old), "a record matches itself");
        assert!(matches(&fresh, &fresh), "a canonical record matches itself");
    }

    #[test]
    fn marks_stacked_in_any_old_order_match_the_canonical_one() {
        let canonical = vec![
            link("https://example.org/"),
            mark("bold"),
            mark("italic"),
            mark("abbr"),
            mark("superscript"),
        ];
        let fresh = paragraph_doc("Doc", "stacked", &canonical);
        for shift in 0..canonical.len() {
            let mut rotated = canonical.clone();
            rotated.rotate_left(shift);
            let old = paragraph_doc("Doc", "stacked", &rotated);
            assert!(matches(&old, &fresh), "rotated by {shift}");
        }
        let mut reversed = canonical.clone();
        reversed.reverse();
        let old = paragraph_doc("Doc", "stacked", &reversed);
        assert!(matches(&old, &fresh), "reversed");
    }

    /// A record of a paragraph inside a blockquote: the second of its two text
    /// runs carries `marks`.
    fn quoted_doc(marks: &[Value]) -> Value {
        json!({
            "title": "Doc",
            "body": "stacked",
            "tiptapJson": {
                "type": "doc",
                "content": [{
                    "type": "blockquote",
                    "attrs": { "data-block-id": "q" },
                    "content": [{
                        "type": "paragraph",
                        "attrs": { "data-block-id": "p" },
                        "content": [
                            { "type": "text", "text": "plain " },
                            { "type": "text", "text": "stacked", "marks": marks },
                        ],
                    }],
                }],
            },
        })
    }

    #[test]
    fn marks_in_an_old_order_inside_nested_blocks_match() {
        let old = quoted_doc(&[mark("italic"), mark("bold")]);
        let fresh = quoted_doc(&[mark("bold"), mark("italic")]);
        assert!(matches(&old, &fresh));
        assert!(!matches(&old, &quoted_doc(&[mark("bold")])));
    }

    // ---- negative controls: these must stay false -------------------------

    #[test]
    fn a_different_mark_set_still_differs() {
        let record = paragraph_doc("Doc", "stacked", &[mark("superscript"), mark("bold")]);
        let others: [Vec<Value>; 4] = [
            vec![mark("bold"), mark("italic")],
            vec![mark("bold")],
            vec![mark("bold"), mark("superscript"), mark("italic")],
            vec![],
        ];
        for other in others {
            let candidate = paragraph_doc("Doc", "stacked", &other);
            assert!(!matches(&record, &candidate), "{other:?}");
            assert!(
                !matches(&candidate, &record),
                "{other:?}, the other way round"
            );
        }
    }

    #[test]
    fn a_mark_with_different_attrs_still_differs() {
        let record = paragraph_doc(
            "Doc",
            "stacked",
            &[link("https://example.org/a"), mark("bold")],
        );
        // The control: the same marks and attrs in the other order are a match.
        let reordered = paragraph_doc(
            "Doc",
            "stacked",
            &[mark("bold"), link("https://example.org/a")],
        );
        assert!(matches(&record, &reordered));
        // The same marks in the other order with another href are not.
        let other_href = paragraph_doc(
            "Doc",
            "stacked",
            &[mark("bold"), link("https://example.org/b")],
        );
        assert!(!matches(&record, &other_href));
        assert!(!matches(&other_href, &record));
    }

    #[test]
    fn different_text_still_differs_even_when_the_mark_order_differs_too() {
        let record = paragraph_doc("Doc", "stacked", &[mark("superscript"), mark("bold")]);
        let other_text = paragraph_doc("Doc", "stackeD", &[mark("bold"), mark("superscript")]);
        assert!(!matches(&record, &other_text));
        let same_order_other_text =
            paragraph_doc("Doc", "stackeD", &[mark("superscript"), mark("bold")]);
        assert!(!matches(&record, &same_order_other_text));
    }

    #[test]
    fn a_different_title_still_differs() {
        let record = paragraph_doc("Doc", "stacked", &[mark("superscript"), mark("bold")]);
        let other_title = paragraph_doc(
            "Another title",
            "stacked",
            &[mark("bold"), mark("superscript")],
        );
        assert!(!matches(&record, &other_title));
        // The control: the same candidate under the record's title matches.
        let same_title = paragraph_doc("Doc", "stacked", &[mark("bold"), mark("superscript")]);
        assert!(matches(&record, &same_title));
    }

    #[test]
    fn a_different_body_or_document_kind_still_differs() {
        let record = paragraph_doc("Doc", "stacked", &[mark("superscript"), mark("bold")]);
        let mut other_body = paragraph_doc("Doc", "stacked", &[mark("bold"), mark("superscript")]);
        other_body["body"] = json!("another body");
        assert!(!matches(&record, &other_body));
        let mut other_kind = paragraph_doc("Doc", "stacked", &[mark("bold"), mark("superscript")]);
        other_kind["documentKind"] = json!("tabula");
        assert!(!matches(&record, &other_kind));
    }

    #[test]
    fn a_null_or_absent_tiptap_json_is_still_compared_exactly() {
        // A flow board's record carries `tiptapJson: null` by construction.
        let board = json!({ "title": "Doc", "body": "stacked", "tiptapJson": null });
        assert!(matches(&board, &board.clone()));
        let text = paragraph_doc("Doc", "stacked", &[mark("bold")]);
        assert!(!matches(&board, &text));
        assert!(!matches(&text, &board));
        // Absent is not null, as before.
        let absent = json!({ "title": "Doc", "body": "stacked" });
        assert!(!matches(&absent, &board));
        assert!(!matches(&board, &absent));
        assert!(matches(&absent, &absent.clone()));
    }
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
    if room.is_evicted() {
        return Err("room was evicted".to_string());
    }
    let document_kind = resolve_document_kind(app, graph_id, document_id).await?;
    let materialized = materialize_room_document(
        graph_id,
        document_id,
        title,
        room,
        operation_id,
        expected_revision,
        document_kind.as_deref(),
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
    if room.is_evicted() {
        return Err("room was evicted".to_string());
    }
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
                let record = crate::document_persistence_service::save_room_document_projection(
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

// ---------------------------------------------------------------------------
// Quote matching that tolerates how a quote was copied. A port of Shrubbery's
// reader overlay (apps/organism/src/cell/source-quote-annotations.ts:
// `canonical`, `QuoteTextBuilder`, `searchFormOf`, `findIn`, `findElided`), so
// a quote the margin finds is a quote gardend anchors: whitespace (soft line
// wraps, NBSP) is one space, curly quotes are straight ones, every dash is
// `-` with no space around it, `…` is `...`, case folds when the exact form
// misses, and an elided quote (`a … b`) matches its pieces in order. Each
// normalized character keeps the UTF-16 range of the source character it
// came from, so a match maps back to the exact original span.
// ---------------------------------------------------------------------------

const QUOTE_MIN_LENGTH: usize = 2;
const QUOTE_MAX_ELISION_GAP: usize = 2400;

/// JavaScript's `\s`.
fn quote_space(c: char) -> bool {
    const SPACES: &[char] = &[
        '\t', '\n', '\u{b}', '\u{c}', '\r', ' ', '\u{a0}', '\u{1680}', '\u{2028}', '\u{2029}',
        '\u{202f}', '\u{205f}', '\u{3000}', '\u{feff}',
    ];
    SPACES.contains(&c) || ('\u{2000}'..='\u{200a}').contains(&c)
}

/// The search form of a text: its normalized characters, their case-folded
/// twins, and for each the UTF-16 `(offset, length)` of its source character.
struct QuoteForm {
    chars: Vec<char>,
    folded: Vec<char>,
    source: Vec<(usize, usize)>,
}

impl QuoteForm {
    fn of(text: &str) -> Self {
        let mut form = Self {
            chars: Vec::new(),
            folded: Vec::new(),
            source: Vec::new(),
        };
        let mut offset = 0usize;
        for c in text.chars() {
            let at = (offset, c.len_utf16());
            if quote_space(c) {
                form.emit(' ', at);
            } else if matches!(c, '\u{ad}' | '\u{200b}'..='\u{200d}' | '\u{2060}') {
                // dropped: soft hyphen, zero-width characters, word joiner
            } else if matches!(c, '‘' | '’' | '‚' | '‛' | '′' | 'ʼ') {
                form.emit('\'', at);
            } else if matches!(c, '“' | '”' | '„' | '‟' | '″' | '«' | '»') {
                form.emit('"', at);
            } else if matches!(c, '\u{2010}'..='\u{2015}' | '\u{2212}') {
                form.emit('-', at);
            } else if c == '…' {
                for _ in 0..3 {
                    form.emit('.', at);
                }
            } else {
                form.emit(c, at);
            }
            offset += c.len_utf16();
        }
        while form.chars.last() == Some(&' ') {
            form.pop();
        }
        form
    }

    /// A quote's form: a spaced ellipsis (`. . .`) first becomes `...`.
    fn of_quote(quote: &str) -> Self {
        let chars: Vec<char> = quote.chars().collect();
        let mut text = String::with_capacity(quote.len());
        let mut index = 0;
        while index < chars.len() {
            if chars[index] == '.' {
                let (mut cursor, mut dots) = (index + 1, 1);
                loop {
                    let mut probe = cursor;
                    while probe < chars.len() && matches!(chars[probe], ' ' | '\t' | '\u{a0}') {
                        probe += 1;
                    }
                    if probe < chars.len() && chars[probe] == '.' {
                        dots += 1;
                        cursor = probe + 1;
                    } else {
                        break;
                    }
                }
                if dots >= 3 {
                    text.push_str("...");
                    index = cursor;
                    continue;
                }
            }
            text.push(chars[index]);
            index += 1;
        }
        Self::of(&text)
    }

    fn last(&self, back: usize) -> Option<char> {
        self.chars
            .len()
            .checked_sub(back)
            .map(|index| self.chars[index])
    }

    fn pop(&mut self) {
        self.chars.pop();
        self.folded.pop();
        self.source.pop();
    }

    fn emit(&mut self, c: char, at: (usize, usize)) {
        if c == ' ' {
            if self.chars.is_empty() || matches!(self.last(1), Some(' ' | '-')) {
                return;
            }
        } else if c == '-' {
            if self.last(1) == Some(' ') {
                self.pop();
            }
            if self.last(1) == Some('-') {
                return;
            }
        } else if c == '.'
            && self.last(1) == Some('.')
            && self.last(2) == Some('.')
            && self.last(3) == Some('.')
        {
            // A fourth dot and beyond: "floor...." keeps three.
            return;
        }
        let mut lower = c.to_lowercase();
        let folded = match (lower.next(), lower.next()) {
            (Some(one), None) => one,
            _ => c,
        };
        self.chars.push(c);
        self.folded.push(folded);
        self.source.push(at);
    }

    /// The UTF-16 `(offset, length)` in the source of normalized `[start, end)`.
    fn source_span(&self, start: usize, end: usize) -> (usize, usize) {
        let (first, _) = self.source[start];
        let (last, last_len) = self.source[end - 1];
        (first, last + last_len - first)
    }
}

fn quote_index_of(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&at| haystack[at..at + needle.len()] == *needle)
}

/// Every non-overlapping match (`findIn`: exact form, else case-folded), or
/// the first elided match (`findElided`), as normalized `[start, end)`.
fn quote_matches(haystack: &QuoteForm, needle: &QuoteForm) -> Vec<(usize, usize)> {
    if needle.chars.len() < QUOTE_MIN_LENGTH {
        return Vec::new();
    }
    for (hay, find) in [
        (&haystack.chars, &needle.chars),
        (&haystack.folded, &needle.folded),
    ] {
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(at) = quote_index_of(hay, find, from) {
            found.push((at, at + find.len()));
            from = at + find.len();
        }
        if !found.is_empty() {
            return found;
        }
    }
    quote_elided(haystack, needle).into_iter().collect()
}

fn quote_elided(haystack: &QuoteForm, needle: &QuoteForm) -> Option<(usize, usize)> {
    let folded = &needle.folded;
    let dots = ['.', '.', '.'];
    quote_index_of(folded, &dots, 0)?;
    let mut pieces: Vec<Vec<char>> = Vec::new();
    let mut cursor = 0;
    while cursor <= folded.len() {
        let end = quote_index_of(folded, &dots, cursor).unwrap_or(folded.len());
        let piece: String = folded[cursor..end].iter().collect();
        let piece: Vec<char> = piece.trim().chars().collect();
        if piece.len() >= QUOTE_MIN_LENGTH {
            pieces.push(piece);
        }
        cursor = end + 3;
    }
    let (first, rest) = pieces.split_first()?;
    let hay = &haystack.folded;
    let mut start = quote_index_of(hay, first, 0);
    while let Some(at) = start {
        let mut cursor = at + first.len();
        let mut complete = true;
        for piece in rest {
            let next = quote_index_of(hay, piece, cursor)?;
            if next - cursor > QUOTE_MAX_ELISION_GAP {
                complete = false;
                break;
            }
            cursor = next + piece.len();
        }
        if complete {
            return Some((at, cursor));
        }
        start = quote_index_of(hay, first, at + 1);
    }
    None
}

/// Where `find` is in `text`, as UTF-16 `(offset, length)` ranges chosen by
/// `occurrence` (0 = all, -1 = the last, n = the nth): the exact text first,
/// as before; else the normalized match, mapped back to the original span.
pub(crate) fn quote_ranges(text: &str, find: &str, occurrence: f64) -> Vec<(usize, usize)> {
    let exact = occurrence_offsets(text, find, occurrence);
    if !exact.is_empty() {
        let length = block_ops::utf16_len(find);
        return exact.into_iter().map(|offset| (offset, length)).collect();
    }
    let haystack = QuoteForm::of(text);
    // Then two more copying habits, as fallbacks in this order: a quote
    // copied as a JSON-escaped string (`\` `n` where the text wraps), and
    // quotation marks the copier put around a sentence the text has bare.
    let unescaped = quote_unescaped_whitespace(find);
    let stripped = quote_without_outer_marks(unescaped.as_deref().unwrap_or(find));
    for candidate in [Some(find), unescaped.as_deref(), stripped.as_deref()]
        .into_iter()
        .flatten()
    {
        let matches = quote_matches(&haystack, &QuoteForm::of_quote(candidate));
        let chosen: Vec<(usize, usize)> = if occurrence == 0.0 {
            matches
        } else if occurrence == -1.0 {
            matches.last().copied().into_iter().collect()
        } else {
            let index = (occurrence.floor().max(1.0) as usize) - 1;
            matches.get(index).copied().into_iter().collect()
        };
        if !chosen.is_empty() {
            return chosen
                .into_iter()
                .map(|(start, end)| haystack.source_span(start, end))
                .collect();
        }
    }
    Vec::new()
}

/// `find` with a literal backslash + `n`, `t` or `r` read as whitespace, when
/// it has one.
fn quote_unescaped_whitespace(find: &str) -> Option<String> {
    const ESCAPES: [&str; 3] = ["\\n", "\\t", "\\r"];
    ESCAPES.iter().any(|escape| find.contains(escape)).then(|| {
        ESCAPES
            .iter()
            .fold(find.to_string(), |text, escape| text.replace(escape, " "))
    })
}

/// `find` without quotation marks and whitespace at either end, when it has
/// any and something is left.
fn quote_without_outer_marks(find: &str) -> Option<String> {
    let outer =
        |c: char| matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’' | '«' | '»') || quote_space(c);
    let stripped = find.trim_matches(outer);
    (stripped.len() != find.len() && stripped.chars().count() >= QUOTE_MIN_LENGTH)
        .then(|| stripped.to_string())
}

/// The first block holding `find`, searching the blocks after `after` (the
/// block a caller named: a passage runs on into the next ones) and then those
/// before it; every block, in order, when `after` is `None`.
pub(crate) fn find_quote_in_blocks<T: ReadTxn>(
    txn: &T,
    fragment: &yrs::XmlFragmentRef,
    after: Option<u32>,
    find: &str,
    occurrence: f64,
) -> Option<(XmlElementRef, String, Vec<(usize, usize)>)> {
    let blocks: Vec<(u32, XmlElementRef)> = fragment
        .children(txn)
        .enumerate()
        .filter_map(|(index, child)| match child {
            XmlOut::Element(element) => Some((index as u32, element)),
            _ => None,
        })
        .collect();
    let order = blocks
        .iter()
        .filter(|(index, _)| after.is_none_or(|after| *index > after))
        .chain(
            blocks
                .iter()
                .filter(|(index, _)| after.is_some_and(|after| *index < after)),
        );
    for (_, element) in order {
        let Some(id) = block_ops::read_block_id_from_element(txn, element) else {
            continue;
        };
        let ranges = quote_ranges(&read_block_text(txn, element), find, occurrence);
        if !ranges.is_empty() {
            return Some((element.clone(), id, ranges));
        }
    }
    None
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
    crate::comment_reader_keys::validate(payload)
        .map_err(|error| format!("document.editComment: {error}"))?;
    // A highlight is a mark with no note: its text may be empty.
    let text = match js_string_value(payload.get("text")) {
        Some(text) => text,
        None if crate::comment_reader_keys::is_highlight(payload, &existing) => String::new(),
        None => {
            return Err("document.editComment: text is required for action=set".to_string());
        }
    };
    // The server signs the comment (crate::comment_authorship): the
    // caller's `author`/`authorId` are not the author (2026-10-14).
    let author = crate::comment_authorship::CommentAuthor::from_payload(payload);
    let quoted_text = js_string_value(coalesce(payload, &["quotedText", "quoted_text"]));
    let block_id = js_string_value(coalesce(payload, &["blockId", "block_id"]));
    let find = js_string_value(payload.get("find"));
    let occurrence = js_finite_number(payload.get("occurrence")).unwrap_or(1.0);
    let mut anchored = 0usize;
    let mut anchor_block_id: Option<String> = None;
    let mut effective_quoted_text = quoted_text;

    if let (Some(block_id), Some(find)) = (&block_id, &find) {
        // The quote as it was copied (soft wraps, typographic quotes and
        // dashes, an elided end) in the named block; else in the blocks
        // after it, then before it. Only a quote no block holds is refused.
        let named = block_ops::find_block_in_fragment(txn, &fragment, block_id);
        let in_named = named.as_ref().and_then(|(element, _)| {
            let ranges = quote_ranges(&read_block_text(txn, element), find, occurrence);
            (!ranges.is_empty()).then(|| (element.clone(), block_id.clone(), ranges))
        });
        let (element, found_in, ranges) = match in_named {
            Some(found) => found,
            None => find_quote_in_blocks(
                txn,
                &fragment,
                named.as_ref().map(|(_, index)| *index),
                find,
                occurrence,
            )
            .ok_or_else(|| match named {
                Some(_) => format!(
                    "text not found in block {block_id} or in any other block of the document: {find}"
                ),
                None => format!("Block not found: {block_id}"),
            })?,
        };
        let mark_attrs = json!({ "commentMark": { "commentId": comment_id } })
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (offset, length) in ranges {
            anchored += format_block_text_range(txn, &element, offset, length, &mark_attrs);
        }
        anchor_block_id = Some(found_in);
        if effective_quoted_text.is_none() {
            effective_quoted_text = Some(find.clone());
        }
    }

    let mut data = existing.clone();
    data.insert("id".to_string(), json!(comment_id));
    data.insert("commentId".to_string(), json!(comment_id));
    data.insert("text".to_string(), json!(text));
    crate::comment_reader_keys::apply(&mut data, payload);
    let stored = (!existing.is_empty()).then_some(&existing);
    crate::comment_authorship::sign_comment(&mut data, &author, payload.get("author"), stored);
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
    edit_comment_with_lease(app, operation, None).await
}

pub(super) async fn edit_comment_with_lease(
    app: &AppHandle,
    operation: &CrdtOperation,
    lease: Option<&mut super::persistence_coordinator::HotWriteLease>,
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

    // The room sidecar is durable. Keep only the lifecycle fence while the
    // per-room projection gate orders record/RDF/history work. Still await
    // that work before success: read_document consumes the cold record.
    if let Some(lease) = lease {
        lease.release_for_projection();
    }
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
// document.editComments: several `set` comments on one document in ONE Y.Doc
// transaction and ONE persist (workflow_plan_publish). A document.editComment
// per comment re-projects the whole document each time; on a 209k-word book
// that was 6-20 s a comment. Each comment anchors exactly as
// document.editComment does (blockId + find + occurrence); one that names no
// blockId anchors at the first block whose text holds its find. A comment
// that cannot anchor is reported and written nowhere.
// ---------------------------------------------------------------------------

pub(crate) async fn edit_comments_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let graph_id = operation.graph_id.clone();
    let document_id = operation
        .document_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "document.editComments: documentId is required".to_string())?;
    let payload = obj(&operation.payload);
    let comments: Vec<Map<String, Value>> = payload
        .get("comments")
        .and_then(Value::as_array)
        .filter(|comments| !comments.is_empty())
        .ok_or_else(|| {
            ApplyOperationError::terminal(
                "document.editComments: comments must be a non-empty list",
            )
        })?
        .iter()
        .map(|comment| comment.as_object().cloned().unwrap_or_default())
        .collect();
    let now = js_finite_number(payload.get("updatedAt")).ok_or_else(|| {
        "document.editComments: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp"
            .to_string()
    })?;
    let graph_dir = existing_graph_dir(app, &graph_id)?;
    let registry = app.state::<RoomRegistry>();
    // Never open (and so create) a room for a document this graph does not hold.
    if !crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
        &registry,
        &graph_id,
        &graph_dir,
        &document_id,
    )
    .await
    .unwrap_or(false)
    {
        let not_found: Vec<Value> = comments
            .iter()
            .map(|comment| {
                json!({
                    "commentId": comment.get("commentId"),
                    "reason": format!("document {document_id} is not in graph {graph_id}"),
                })
            })
            .collect();
        return Ok(json!({
            "documentId": document_id,
            "graphId": graph_id,
            "placed": [],
            "notFound": not_found,
        }));
    }
    let room = registry
        .get_or_create(
            &format!("doc:{graph_id}:{document_id}"),
            checked_document_ydoc_state_path(&graph_dir, &document_id)?,
        )
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    // What every comment shares: the server-stamped author and the caller's
    // claimed one (crate::comment_authorship).
    let mut shared = payload.clone();
    shared.remove("comments");
    let outcome = room
        .update_doc(move |_doc, txn| {
            let fragment = txn.get_or_insert_xml_fragment("content");
            let mut placed = Vec::new();
            let mut not_found = Vec::new();
            for comment in comments {
                let mut fields = shared.clone();
                fields.extend(comment);
                fields.insert("action".to_string(), json!("set"));
                let comment_id = js_string_value(coalesce(&fields, &["commentId", "comment_id"]));
                let Some(comment_id) = comment_id else {
                    not_found.push(json!({ "reason": "a comment has no commentId" }));
                    continue;
                };
                let has_block =
                    js_string_value(coalesce(&fields, &["blockId", "block_id"])).is_some();
                if !has_block {
                    let Some(find) = js_string_value(fields.get("find")) else {
                        not_found
                            .push(json!({ "commentId": comment_id, "reason": "nothing to find" }));
                        continue;
                    };
                    let holder = find_quote_in_blocks(txn, &fragment, None, &find, 1.0)
                        .map(|(_, block_id, _)| block_id);
                    match holder {
                        Some(block_id) => {
                            fields.insert("blockId".to_string(), json!(block_id));
                        }
                        None => {
                            not_found.push(json!({
                                "commentId": comment_id,
                                "reason": format!("text not found in the document: {find}"),
                            }));
                            continue;
                        }
                    }
                }
                match edit_comment_in_doc(txn, &fields, "set", &comment_id, now) {
                    Ok(result) => placed.push(result),
                    Err(error) => {
                        not_found.push(json!({ "commentId": comment_id, "reason": error }))
                    }
                }
            }
            Ok::<_, String>((placed, not_found))
        })
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    let (placed, not_found) = outcome;
    if !placed.is_empty() {
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
    }
    Ok(json!({
        "documentId": document_id,
        "graphId": graph_id,
        "placed": placed,
        "notFound": not_found,
    }))
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
    // Every failure of the create below is retried, so the refusals it would
    // make of this id (a deleted document's) are made here: a caller-supplied
    // tombstoned id parked the ingest, and every later write behind it, forever.
    crate::document_service::refuse_uncreatable_document_id(&graph_dir, &document_id)?;
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
    // Parsed markdown is already canonical; the contract normaliser runs on
    // every JSON write path regardless (a no-op here unless the converter
    // emitted a node the contract does not know).
    let (content_nodes, contract_warnings) = super::block_contract::normalise_doc_content(
        parsed
            .tiptap_json
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        &operation.operation_id,
    );
    // A re-ingest into the same id changes only what differs: the parser mints
    // every id of a markdown parse, so a matched block keeps its current id, and
    // what markdown cannot say (a comment mark, an outline state) stays.
    let mut minted = std::collections::BTreeSet::new();
    collect_block_ids(&content_nodes, &mut minted);
    let policy = WritePolicy {
        format: SourceFormat::Markdown,
        ids: IdPolicy::Adopt { minted },
    };
    let desired = minimal_edit::model::desired_from_tiptap_nodes(&content_nodes, &policy);
    room.update_doc(move |_doc, txn| {
        let fragment = txn.get_or_insert_xml_fragment("content");
        let current = minimal_edit::model::read_fragment(&*txn, &fragment, &policy);
        let desired = keep_trailing_empty_paragraph(&current, desired.clone(), &policy);
        let plan = minimal_edit::plan::plan_fragment(&current, &desired, &policy);
        minimal_edit::emit::emit_fragment(txn, &fragment, &current, &plan)?;
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
    warnings.extend(contract_warnings);

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
