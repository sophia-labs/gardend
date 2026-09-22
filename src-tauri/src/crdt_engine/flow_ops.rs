//! `flow.seed` CRDT op + the board export core (unit G4, after G3).
//!
//! **`flow.seed`** `{ json, replace? }` (graph from the operation envelope):
//! ensure the `flow-board` workspace entry exists with
//! `documentKind:"flow-board"` (created through the existing
//! `workspace.createDocument` doc-level path — `write_workspace_document` —
//! with the extra field; title "Flow board"), then REPLACE the board room's
//! state with `flow_board::seed_board_txn` in ONE room transaction: seeding
//! is a whole-board act (FLOW-UNDO-11: seed = fresh history — server-side
//! there is no undo stack to clear, and clients' per-connection UndoManagers
//! never track these remote transactions, so the seed arrives un-undoable by
//! construction). A non-empty board refuses without `replace:true`. The
//! persistence tail (`persist_room_document`) runs inline, so the enqueue's
//! completion IS the flush (the MCP handler still waits on
//! `flush_graph_projection` as belt-and-braces).
//!
//! Scope rule: like the other `document.*` writes — `documents.write.crdt`
//! (`loopback_scopes::CRDT_OPERATION_SCOPE_RULES`).
//!
//! **`export_board`** (the `flow_export_board` MCP core): without a restore
//! point, read the LIVE room under `Room::with_doc` — the doc lock makes the
//! whole export one consistency point (GEOM-13: a torn read is impossible by
//! construction). With one, decode that restore point's persisted board bytes
//! (`time_travel_store::read_document_bytes`) into a scratch `Doc` and export
//! with the id in the `sophia` sidecar — the id travels in the file
//! (FLOW-GARDEN-P1).

use crate::app_runtime::AppHandle;
use crate::crdt_queue::{CrdtOperation, RECOVERED_OPERATION_PAYLOAD_KEY};
use crate::flow_board::{
    board_derive_report, board_doc_from_update_bytes, board_export_json, board_rows, cycles_json,
    derive_report_json, seed_board_txn, BOARD_DOCUMENT_ID, FLOW_BOARD_KIND,
};
use serde_json::{json, Map as JsonMap, Value};
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::{Map as YMap, ReadTxn, WriteTxn};

use super::executor::{ApplyOperationError, ApplyOperationResult};
use super::rooms::RoomRegistry;

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let mut hot_committed = false;
    seed_board(app, operation, &mut hot_committed)
        .await
        .map_err(|error| {
            if hot_committed {
                ApplyOperationError::retryable_after_hot_commit(error)
            } else {
                ApplyOperationError::terminal(error)
            }
        })
}

async fn seed_board(
    app: &AppHandle,
    operation: &CrdtOperation,
    hot_committed: &mut bool,
) -> Result<Value, String> {
    let graph_id = operation.graph_id.clone();
    let graph_dir = crate::graph_paths::existing_graph_dir(app, &graph_id)?;
    let payload = &operation.payload;

    let json_text = payload
        .get("json")
        .and_then(Value::as_str)
        .ok_or_else(|| "flow.seed: json (the Flow board file, as a string) is required".to_string())?
        .to_string();
    // A recovered replay of a committed seed re-runs against the board it
    // already seeded; treating it as replace keeps recovery idempotent
    // (same journaled JSON in, same board out) instead of wedging on the
    // non-empty refusal below.
    let recovered = payload
        .get(RECOVERED_OPERATION_PAYLOAD_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let replace = payload
        .get("replace")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || recovered;

    // Validate the input and precompute the seed stats BEFORE touching any
    // room: a replace must never destroy a board over unparseable JSON, and
    // the stats of the validated input are — by construction — the stats of
    // the room after the seed (same `seed_board_txn` body, same input).
    let validated =
        crate::flow_board::board_from_json(&json_text).map_err(|e| format!("flow.seed: {e}"))?;
    let rows = board_rows(&validated);
    let report = board_derive_report(&rows);
    let mut row_counts = JsonMap::new();
    let mut subjects: usize = 0;
    for (json_key, ddl_table, _, _) in crate::flow_board::table_order() {
        let count = rows
            .tables
            .get(*ddl_table)
            .map(|t| t.rows.len())
            .unwrap_or(0);
        subjects += count;
        row_counts.insert((*json_key).to_string(), json!(count));
    }
    let extras_keys: Vec<&str> = rows.extras.iter().map(|(k, _)| k.as_str()).collect();
    let extras_json = json!(extras_keys);

    // Deterministic workspace-entry timestamps: derived from the journaled
    // enqueue timestamp exactly like `normalize_payload_ids` does for
    // workspace.createDocument, so recovery replays the same entry.
    let entry_ts: u64 = operation
        .enqueue_timestamp
        .parse()
        .map_err(|_| "flow.seed: operation enqueue timestamp is not epoch-ms".to_string())?;

    // ── Ensure the workspace entry `flow-board` with the kind flag ────────
    let ws_room = super::workspace_ops::workspace_room(app, &graph_id, &graph_dir).await?;
    let (entry_exists, entry_kind) = ws_room
        .with_doc(move |doc| {
            (
                super::workspace_ops::document_exists_in_workspace(doc, BOARD_DOCUMENT_ID),
                super::workspace_ops::document_kind(doc, BOARD_DOCUMENT_ID),
            )
        })
        .await;
    match (entry_exists, entry_kind.as_deref()) {
        (true, Some(FLOW_BOARD_KIND)) => {}
        (true, other) => {
            return Err(format!(
                "flow.seed: document '{BOARD_DOCUMENT_ID}' already exists in this graph but is \
                 not a flow board (documentKind: {other:?}) — refusing to seed over it"
            ));
        }
        (false, _) => {
            super::workspace_ops::upsert_document_entry(
                app,
                &graph_id,
                &json!({
                    "documentId": BOARD_DOCUMENT_ID,
                    "title": "Flow board",
                    "documentKind": FLOW_BOARD_KIND,
                    "order": entry_ts,
                    "updatedAt": entry_ts,
                }),
                &operation.operation_id,
            )
            .await?;
            *hot_committed = true;
        }
    }

    // ── The whole-board room write: one transaction ───────────────────────
    let registry = app.state::<RoomRegistry>();
    let room = registry
        .get_or_create(
            &format!("doc:{graph_id}:{BOARD_DOCUMENT_ID}"),
            crate::ydoc_paths::document_ydoc_state_path(&graph_dir, BOARD_DOCUMENT_ID),
        )
        .await?;
    room.update_doc(move |_doc, txn| {
        let non_empty = ["resource", "scene"].iter().any(|root| {
            txn.get_map(*root)
                .map(|map| map.len(&*txn) > 0)
                .unwrap_or(false)
        });
        if non_empty && !replace {
            return Err(
                "flow.seed: the board room already has content; seeding is a whole-board act — \
                 pass replace:true to replace the entire board"
                    .to_string(),
            );
        }
        // REPLACE: clear both named roots, then seed — same transaction, so
        // clients see one atomic remote update and no intermediate state.
        for root in ["resource", "scene"] {
            let map = txn.get_or_insert_map(root);
            let keys: Vec<String> = map.keys(&*txn).map(str::to_string).collect();
            for key in keys {
                map.remove(txn, &key);
            }
        }
        seed_board_txn(txn, &json_text).map_err(|e| format!("flow.seed: {e}"))
    })
    .await?;
    *hot_committed = true;

    // ── Persistence tail: record + history + `:projection:flow` reconcile ─
    let title = ws_room
        .with_doc(|doc| {
            super::workspace_ops::document_title_in_workspace(doc, BOARD_DOCUMENT_ID)
        })
        .await
        .unwrap_or_else(|| "Flow board".to_string());
    let flushed = super::document_ops::persist_room_document(
        app,
        &graph_id,
        BOARD_DOCUMENT_ID,
        &title,
        &room,
        &operation.operation_id,
    )
    .await?;

    Ok(json!({
        "documentId": BOARD_DOCUMENT_ID,
        "graphId": graph_id,
        "rows": row_counts,
        "subjects": subjects,
        "extras": extras_json,
        "derive": derive_report_json(&report),
        "revision": flushed.get("revision").cloned().unwrap_or(Value::Null),
    }))
}

/// The `flow_export_board` core. `restore_point_id: None` → live room export
/// (one consistency point); `Some(id)` → that restore point's board bytes,
/// id in the sidecar (FLOW-GARDEN-P1).
pub(crate) async fn export_board(
    app: &AppHandle,
    graph_id: &str,
    restore_point_id: Option<&str>,
) -> Result<Value, String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let kind = super::document_ops::resolve_document_kind(app, graph_id, BOARD_DOCUMENT_ID).await?;
    if kind.as_deref() != Some(FLOW_BOARD_KIND) {
        return Err(format!(
            "flow_export_board: graph {graph_id} has no flow board (no workspace entry \
             '{BOARD_DOCUMENT_ID}' with documentKind \"{FLOW_BOARD_KIND}\") — seed one with \
             flow_seed_board first"
        ));
    }

    let export = match restore_point_id {
        Some(rp_id) => {
            let bytes =
                crate::time_travel_store::read_document_bytes(&graph_dir, rp_id, BOARD_DOCUMENT_ID)
                    .map_err(|error| {
                        format!("flow_export_board: restore point {rp_id} board bytes: {error}")
                    })?;
            let doc = board_doc_from_update_bytes(&bytes)
                .map_err(|error| format!("flow_export_board: {error}"))?;
            board_export_json(
                &doc,
                Some(crate::flow_board::SophiaSidecar {
                    restore_point_id: Some(rp_id.to_string()),
                    graph_id: graph_id.to_string(),
                }),
            )
        }
        None => {
            let registry = app.state::<RoomRegistry>();
            let room = registry
                .get_or_create(
                    &format!("doc:{graph_id}:{BOARD_DOCUMENT_ID}"),
                    crate::ydoc_paths::document_ydoc_state_path(&graph_dir, BOARD_DOCUMENT_ID),
                )
                .await?;
            let sidecar_graph = graph_id.to_string();
            room.with_doc(move |doc| {
                board_export_json(
                    doc,
                    Some(crate::flow_board::SophiaSidecar {
                        restore_point_id: None,
                        graph_id: sidecar_graph,
                    }),
                )
            })
            .await
        }
    };

    Ok(json!({
        "json": export.json,
        "derive": derive_report_json(&export.report),
        "cycles": cycles_json(&export.report.cycles),
    }))
}
