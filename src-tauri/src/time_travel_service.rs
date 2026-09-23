use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    clock,
    document_history_file_store::read_document_snapshot_payload,
    document_history_service::current_document_snapshot_with_lease,
    graph_paths::existing_graph_dir,
    storage::read_json,
    storage_file_ops::read_bytes,
    time_travel_paths::{
        document_bytes_relative_path, RESTORE_POINT_WORKSPACE_BYTES_FILE,
        RESTORE_POINT_WORKSPACE_SNAPSHOT_FILE,
    },
    time_travel_store::{
        delete_restore_point, list_index_page, read_manifest, read_workspace_bundle,
        remove_index_entry, upsert_index_entry, write_document_bytes, write_manifest,
        write_workspace_bundle, ListPage,
    },
    time_travel_types::{
        DocumentSnapshotRef, RestorePointIndexEntry, RestorePointManifest, RestorePointMetadata,
        RestorePointTrigger, WorkspaceSnapshotRef, RESTORE_POINT_MANIFEST_SCHEMA_VERSION,
    },
    ydoc_paths::{document_ydoc_state_path, workspace_snapshot_path, workspace_ydoc_state_path},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use uuid::Uuid;

const TIMESTAMP_LABEL: &str = "restore-point";

pub(crate) fn capture_restore_point(
    app: &AppHandle,
    graph_id: &str,
    trigger: RestorePointTrigger,
    label: Option<String>,
) -> AppResult<Value> {
    let lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(AppError::storage)?;
    let result = capture_restore_point_with_lease(app, graph_id, trigger, label);
    if result.is_ok() {
        if let Some(lease) = lease.as_ref() {
            // Capture writes history/restore-bundle files only. Its bounded
            // legacy fallback may backfill a Y.Doc sidecar. A headless ghost
            // self-heal can also create/materialize one document, but that
            // path precisely calls `mark_rdf_store_written`; no possible RDF
            // write in this scope relies on the lease's blanket fallback.
            lease.declare_rdf_writes_self_tracked();
        }
    }
    let _lease = lease;
    result
}

/// Capture while the caller already owns the graph persistence lease.
///
/// Restore uses this for its pre-apply backup so the single lease spans
/// backup + file replacement + RDF rebuild without a re-entrant deadlock.
pub(crate) fn capture_restore_point_with_lease(
    app: &AppHandle,
    graph_id: &str,
    trigger: RestorePointTrigger,
    label: Option<String>,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::not_found)?;
    let restore_point_id = format!("rp-{}", Uuid::new_v4().simple());
    let created_at = clock::epoch_millis() as i64;

    // Workspace bytes + JSON snapshot are best-effort: a freshly-created graph
    // may not have either yet. We persist whatever exists so the bundle is
    // recoverable even from an empty graph.
    let workspace_bytes = read_workspace_ydoc_bytes(&graph_dir)?;
    let workspace_snapshot = read_workspace_snapshot(&graph_dir)?;

    write_workspace_bundle(
        &graph_dir,
        &restore_point_id,
        &workspace_bytes,
        &workspace_snapshot,
    )?;

    let workspace_size = workspace_bytes.len() as u64;
    let workspace_ref = WorkspaceSnapshotRef {
        bytes_path: RESTORE_POINT_WORKSPACE_BYTES_FILE.to_string(),
        snapshot_path: RESTORE_POINT_WORKSPACE_SNAPSHOT_FILE.to_string(),
        size_bytes: workspace_size,
    };

    let document_ids = workspace_document_ids(&workspace_snapshot);
    let mut documents = Vec::with_capacity(document_ids.len());
    let mut total_doc_size: u64 = 0;
    let snapshot_label = label
        .clone()
        .or_else(|| Some(format!("{TIMESTAMP_LABEL}/{restore_point_id}")));

    for document_id in document_ids {
        // Catalog ghosts: a workspace can list a document that has no
        // record on disk (source-side inconsistency, faithfully carried by
        // graph import — hosted catalogs do this). Nothing exists to
        // capture, so nothing can be lost by skipping; failing the WHOLE
        // graph capture here re-ran the full snapshot storm every interval
        // against the same ghost, forever (2026-08-30). Every other
        // per-document failure below still fails the capture closed.
        if !graph_dir
            .join("documents")
            .join(&document_id)
            .join("document.json")
            .is_file()
        {
            log::warn!(
                "restore point {restore_point_id}: skipping catalog ghost {document_id} \
                 (workspace lists it, no document record exists)"
            );
            continue;
        }
        let snapshot_meta = current_document_snapshot_with_lease(
            app,
            graph_id,
            &document_id,
            true,
            snapshot_label.clone(),
        )
        .map_err(AppError::storage)?;
        let payload =
            read_document_snapshot_payload(&graph_dir, &document_id, &snapshot_meta.snapshot_id)
                .map_err(AppError::storage)?;
        let char_count = payload.tiptap_xml.chars().count() as u64;
        let block_count = payload.blocks.len() as u64;
        let title = payload.title.clone();
        // v2 schema: capture the live Y.Doc bytes so restore can be a deterministic
        // file swap rather than a TS-side reconstruction from the snapshot payload.
        let ydoc_state_path = document_ydoc_state_path(&graph_dir, &document_id);
        let ydoc_bytes = if ydoc_state_path.is_file() {
            read_bytes(&ydoc_state_path).map_err(AppError::storage)?
        } else {
            // LEGACY inline-only records: `current_document_snapshot` now
            // reads COLD (no sidecar hydration, no legacy backfill), so a
            // record that predates the sidecar migration may still lack the
            // file here. One hydrating read of THIS document recovers its
            // update payload (and runs the ordinary legacy backfill as a
            // side effect) — bounded to the rare legacy document instead of
            // paid for every document on every capture. FAIL CLOSED: a
            // fallback failure fails the capture exactly like every other
            // per-document failure above — never write an empty payload
            // while the manifest advertises a present `ydoc_bytes_path` (an
            // accepted-but-empty restore point is silent data loss at
            // restore time).
            let record = crate::document_service::read_document_with_lease(
                app.clone(),
                graph_id.to_string(),
                document_id.clone(),
            )
            .map_err(|error| {
                AppError::storage(format!(
                    "capture Y.Doc fallback read for {document_id}: {error}"
                ))
            })?;
            if record.ydoc_update_base64.is_empty() {
                // An empty inline payload is honest ONLY for a provably
                // CONTENT-EMPTY record. `restore_document_metadata`
                // deliberately CLEARS the inline payload on restored (edited!)
                // documents — if such a document's sidecar later goes missing,
                // an empty payload here would silently discard its real
                // history behind a present `ydoc_bytes_path`. Gate on CONTENT
                // emptiness, NEVER on `revision`: `save_document_with_lease`
                // → `save_document_with_tombstone_fence` bumps `revision`
                // UNCONDITIONALLY (even a metadata-only write such as a
                // workspace title sync), so a legitimately-empty document that
                // was merely RENAMED reaches `revision >= 1` with no content —
                // a revision gate would wrongly reject it and fail its
                // capture. Refuse only when some content signal is non-empty;
                // the operator re-flushes to regenerate the sidecar.
                let provably_content_empty = record.blocks.is_empty()
                    && record.body.trim().is_empty()
                    && record.tiptap_xml.trim().is_empty()
                    && record.tree.is_none();
                if !provably_content_empty {
                    return Err(AppError::storage(format!(
                        "capture Y.Doc fallback for {document_id}: record carries content but \
                         both the sidecar and the inline update are missing — refusing to \
                         write an empty restore payload; re-flush the document to regenerate \
                         its Y.Doc sidecar"
                    )));
                }
                Vec::new()
            } else {
                use base64::{engine::general_purpose::STANDARD, Engine as _};
                STANDARD
                    .decode(record.ydoc_update_base64.as_bytes())
                    .map_err(|error| {
                        AppError::storage(format!(
                            "capture Y.Doc fallback decode for {document_id}: {error}"
                        ))
                    })?
            }
        };
        write_document_bytes(&graph_dir, &restore_point_id, &document_id, &ydoc_bytes)?;
        let doc_size = char_count + payload.tiptap_xml.len() as u64 + ydoc_bytes.len() as u64;
        total_doc_size += doc_size;
        documents.push(DocumentSnapshotRef {
            document_id: document_id.clone(),
            title,
            snapshot_id: snapshot_meta.snapshot_id,
            size_bytes: doc_size,
            block_count,
            char_count,
            ydoc_bytes_path: Some(document_bytes_relative_path(&document_id)),
        });
    }

    let metadata = RestorePointMetadata {
        folder_count: workspace_array_len(&workspace_snapshot, "folders"),
        document_count: documents.len() as u64,
        artifact_count: workspace_array_len(&workspace_snapshot, "artifacts"),
        wire_count: workspace_array_len(&workspace_snapshot, "wires"),
        workspace_size_bytes: workspace_size,
        total_document_size_bytes: total_doc_size,
    };

    let mut manifest = RestorePointManifest {
        schema_version: RESTORE_POINT_MANIFEST_SCHEMA_VERSION,
        restore_point_id: restore_point_id.clone(),
        graph_id: graph_id.to_string(),
        created_at,
        trigger,
        label,
        content_hash_sha256: String::new(),
        workspace: workspace_ref,
        documents,
        metadata,
    };
    manifest.content_hash_sha256 = manifest_content_hash(&manifest);
    write_manifest(&graph_dir, &manifest)?;

    let entry = manifest_to_index_entry(&manifest);
    upsert_index_entry(&graph_dir, graph_id, entry.clone())?;

    Ok(restore_point_summary_json(&entry, true))
}

pub(crate) fn list_restore_points_response(
    app: &AppHandle,
    graph_id: &str,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::not_found)?;
    let ListPage {
        entries,
        next_cursor,
        total_count,
    } = list_index_page(&graph_dir, graph_id, cursor, limit)?;
    let summaries: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            // The index is sorted newest-first, and pagination is server-driven,
            // so isLatest is true only for the head entry on the first page.
            let is_latest = idx == 0 && cursor.is_none();
            restore_point_summary_json(entry, is_latest)
        })
        .collect();
    Ok(json!({
        "restorePoints": summaries,
        "nextCursor": next_cursor,
        "totalCount": total_count,
    }))
}

pub(crate) fn get_restore_point_response(
    app: &AppHandle,
    graph_id: &str,
    restore_point_id: &str,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::not_found)?;
    let manifest = read_manifest(&graph_dir, restore_point_id)?;
    Ok(manifest_json(&manifest))
}

pub(crate) fn restore_point_workspace_payload(
    app: &AppHandle,
    graph_id: &str,
    restore_point_id: &str,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::not_found)?;
    let manifest = read_manifest(&graph_dir, restore_point_id)?;
    let (ydoc_bytes, snapshot) = read_workspace_bundle(&graph_dir, restore_point_id)?;
    Ok(json!({
        "restorePointId": manifest.restore_point_id,
        "graphId": manifest.graph_id,
        "createdAt": iso_timestamp(manifest.created_at),
        "workspace": {
            "ydocUpdateBase64": base64_encode(&ydoc_bytes),
            "snapshot": snapshot,
        },
        "documents": manifest
            .documents
            .iter()
            .map(|doc| json!({
                "documentId": doc.document_id,
                "title": doc.title,
                "snapshotId": doc.snapshot_id,
                "blockCount": doc.block_count,
                "charCount": doc.char_count,
            }))
            .collect::<Vec<_>>(),
    }))
}

pub(crate) fn diff_restore_points_response(
    app: &AppHandle,
    graph_id: &str,
    restore_point_id: &str,
    against: Option<&str>,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::not_found)?;
    let target = read_manifest(&graph_dir, restore_point_id)?;
    let against_label = against.unwrap_or("current");

    let baseline_summary = if against_label == "current" {
        // Capture an ephemeral snapshot of "now" without writing it to disk.
        // We just reuse the live workspace bytes + a synthetic baseline view.
        json!({
            "restorePointId": "current",
            "createdAt": iso_timestamp(clock::epoch_millis() as i64),
        })
    } else {
        let baseline = read_manifest(&graph_dir, against_label)?;
        json!({
            "restorePointId": baseline.restore_point_id,
            "createdAt": iso_timestamp(baseline.created_at),
            "label": baseline.label,
            "trigger": baseline.trigger.as_str(),
        })
    };

    let aggregate = if against_label == "current" {
        diff_against_current(app, graph_id, &target)?
    } else {
        let baseline = read_manifest(&graph_dir, against_label)?;
        diff_two_manifests(&target, &baseline)
    };

    Ok(json!({
        "restorePointId": target.restore_point_id,
        "against": against_label,
        "target": json!({
            "restorePointId": target.restore_point_id,
            "createdAt": iso_timestamp(target.created_at),
            "label": target.label,
            "trigger": target.trigger.as_str(),
        }),
        "baseline": baseline_summary,
        "aggregate": aggregate,
    }))
}

pub(crate) fn delete_restore_point_response(
    app: &AppHandle,
    graph_id: &str,
    restore_point_id: &str,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::not_found)?;
    let _ = read_manifest(&graph_dir, restore_point_id)?;
    delete_restore_point(&graph_dir, restore_point_id)?;
    remove_index_entry(&graph_dir, graph_id, restore_point_id)?;
    Ok(json!({
        "restorePointId": restore_point_id,
        "deleted": true,
    }))
}

fn diff_two_manifests(target: &RestorePointManifest, baseline: &RestorePointManifest) -> Value {
    let mut docs_added: u64 = 0;
    let mut docs_removed: u64 = 0;
    let mut docs_modified: u64 = 0;
    let mut total_char_delta: i64 = 0;

    let baseline_by_id: std::collections::BTreeMap<&str, &DocumentSnapshotRef> = baseline
        .documents
        .iter()
        .map(|d| (d.document_id.as_str(), d))
        .collect();
    let target_by_id: std::collections::BTreeMap<&str, &DocumentSnapshotRef> = target
        .documents
        .iter()
        .map(|d| (d.document_id.as_str(), d))
        .collect();
    for (id, target_doc) in &target_by_id {
        match baseline_by_id.get(id) {
            None => {
                docs_added += 1;
                total_char_delta += target_doc.char_count as i64;
            }
            Some(baseline_doc) => {
                if baseline_doc.snapshot_id != target_doc.snapshot_id {
                    docs_modified += 1;
                    total_char_delta +=
                        target_doc.char_count as i64 - baseline_doc.char_count as i64;
                }
            }
        }
    }
    for (id, baseline_doc) in &baseline_by_id {
        if !target_by_id.contains_key(id) {
            docs_removed += 1;
            total_char_delta -= baseline_doc.char_count as i64;
        }
    }
    json!({
        "docsAdded": docs_added,
        "docsRemoved": docs_removed,
        "docsModified": docs_modified,
        "totalCharDelta": total_char_delta,
        "workspaceSizeDelta": target.metadata.workspace_size_bytes as i64
            - baseline.metadata.workspace_size_bytes as i64,
    })
}

fn diff_against_current(
    app: &AppHandle,
    graph_id: &str,
    target: &RestorePointManifest,
) -> AppResult<Value> {
    // "current" diff is a coarse comparison: count current documents in the
    // workspace snapshot and char-size them via the latest persisted snapshot
    // payloads, without writing a new restore point.
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::not_found)?;
    let current_workspace = read_workspace_snapshot(&graph_dir)?;
    let current_doc_ids = workspace_document_ids(&current_workspace);
    let target_ids: std::collections::BTreeSet<&str> = target
        .documents
        .iter()
        .map(|d| d.document_id.as_str())
        .collect();
    let current_set: std::collections::BTreeSet<&str> =
        current_doc_ids.iter().map(|s| s.as_str()).collect();

    let docs_in_current_only = current_set.difference(&target_ids).count() as u64;
    let docs_in_target_only = target_ids.difference(&current_set).count() as u64;
    let docs_in_both = current_set.intersection(&target_ids).count() as u64;

    Ok(json!({
        "docsAdded": docs_in_target_only,
        "docsRemoved": docs_in_current_only,
        "docsModifiedApprox": docs_in_both,
        "comparison": "approximate",
        "note": "diff against `current` reports document set deltas only; capture a restore point first for exact char-level deltas",
    }))
}

fn read_workspace_ydoc_bytes(graph_dir: &Path) -> AppResult<Vec<u8>> {
    let path = workspace_ydoc_state_path(graph_dir);
    if path.is_file() {
        read_bytes(&path).map_err(AppError::storage)
    } else {
        Ok(Vec::new())
    }
}

fn read_workspace_snapshot(graph_dir: &Path) -> AppResult<Value> {
    let path = workspace_snapshot_path(graph_dir);
    if path.is_file() {
        read_json::<Value>(&path).map_err(AppError::storage)
    } else {
        Ok(Value::Null)
    }
}

fn workspace_document_ids(snapshot: &Value) -> Vec<String> {
    snapshot
        .get("documents")
        .and_then(Value::as_array)
        .map(|docs| {
            docs.iter()
                .filter_map(|doc| {
                    doc.get("documentId")
                        .or_else(|| doc.get("document_id"))
                        .or_else(|| doc.get("id"))
                        .and_then(Value::as_str)
                        .map(|value| value.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn workspace_array_len(snapshot: &Value, key: &str) -> u64 {
    snapshot
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| arr.len() as u64)
        .unwrap_or(0)
}

fn manifest_to_index_entry(manifest: &RestorePointManifest) -> RestorePointIndexEntry {
    let total_size = manifest.workspace.size_bytes
        + manifest
            .documents
            .iter()
            .map(|doc| doc.size_bytes)
            .sum::<u64>();
    RestorePointIndexEntry {
        restore_point_id: manifest.restore_point_id.clone(),
        created_at: manifest.created_at,
        trigger: manifest.trigger,
        label: manifest.label.clone(),
        content_hash_sha256: manifest.content_hash_sha256.clone(),
        size_bytes: total_size,
        document_count: manifest.metadata.document_count,
        folder_count: manifest.metadata.folder_count,
        artifact_count: manifest.metadata.artifact_count,
    }
}

fn manifest_content_hash(manifest: &RestorePointManifest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(manifest.graph_id.as_bytes());
    hasher.update(manifest.restore_point_id.as_bytes());
    hasher.update(manifest.created_at.to_le_bytes());
    hasher.update(manifest.workspace.size_bytes.to_le_bytes());
    for doc in &manifest.documents {
        hasher.update(doc.document_id.as_bytes());
        hasher.update(doc.snapshot_id.as_bytes());
        hasher.update(doc.char_count.to_le_bytes());
    }
    let bytes = hasher.finalize();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn restore_point_summary_json(entry: &RestorePointIndexEntry, is_latest: bool) -> Value {
    json!({
        "restorePointId": entry.restore_point_id,
        "timestamp": iso_timestamp(entry.created_at),
        "trigger": entry.trigger.as_str(),
        "label": entry.label,
        "isLatest": is_latest,
        "sizeBytes": entry.size_bytes,
        "contentHashSha256": entry.content_hash_sha256,
        "documentCount": entry.document_count,
        "folderCount": entry.folder_count,
        "artifactCount": entry.artifact_count,
    })
}

fn manifest_json(manifest: &RestorePointManifest) -> Value {
    json!({
        "restorePointId": manifest.restore_point_id,
        "graphId": manifest.graph_id,
        "createdAt": iso_timestamp(manifest.created_at),
        "trigger": manifest.trigger.as_str(),
        "label": manifest.label,
        "contentHashSha256": manifest.content_hash_sha256,
        "workspace": json!({
            "sizeBytes": manifest.workspace.size_bytes,
            "bytesPath": manifest.workspace.bytes_path,
            "snapshotPath": manifest.workspace.snapshot_path,
        }),
        "documents": manifest
            .documents
            .iter()
            .map(|doc| json!({
                "documentId": doc.document_id,
                "title": doc.title,
                "snapshotId": doc.snapshot_id,
                "sizeBytes": doc.size_bytes,
                "blockCount": doc.block_count,
                "charCount": doc.char_count,
            }))
            .collect::<Vec<_>>(),
        "metadata": json!({
            "folderCount": manifest.metadata.folder_count,
            "documentCount": manifest.metadata.document_count,
            "artifactCount": manifest.metadata.artifact_count,
            "wireCount": manifest.metadata.wire_count,
            "workspaceSizeBytes": manifest.metadata.workspace_size_bytes,
            "totalDocumentSizeBytes": manifest.metadata.total_document_size_bytes,
        }),
    })
}

fn iso_timestamp(epoch_ms: i64) -> String {
    // ISO-8601 UTC with millisecond precision, no external chrono dep needed.
    let secs = epoch_ms.div_euclid(1000);
    let millis = epoch_ms.rem_euclid(1000);
    let datetime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.max(0) as u64);
    let _ = datetime; // placeholder; format via simple epoch math below
    format_iso(epoch_ms.max(0), millis)
}

fn format_iso(epoch_ms: i64, millis: i64) -> String {
    // Compute calendar fields from epoch seconds without external deps.
    let mut s = epoch_ms.div_euclid(1000);
    let mut hours = s.div_euclid(3600);
    s -= hours * 3600;
    let minutes = s.div_euclid(60);
    let seconds = s - minutes * 60;
    let days_since_epoch = hours.div_euclid(24);
    hours -= days_since_epoch * 24;
    let (year, month, day) = days_to_ymd(days_since_epoch);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

fn days_to_ymd(days_since_epoch: i64) -> (i64, u32, u32) {
    // Civil-from-days algorithm (Howard Hinnant), valid for any signed day index.
    let z = days_since_epoch + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y_final = if m <= 2 { y + 1 } else { y };
    (y_final, m, d)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use serde_json::json;
    #[cfg(feature = "desktop")]
    use tauri::Manager;

    #[test]
    fn iso_timestamp_format_is_well_formed() {
        let ts = iso_timestamp(0);
        assert_eq!(ts, "1970-01-01T00:00:00.000Z");
        let ts = iso_timestamp(1_700_000_000_000);
        assert_eq!(ts.len(), 24);
        assert!(ts.ends_with("Z"));
        assert!(ts.starts_with("2023-"));
    }

    /// Regression for the interval scheduler's 30-minute detonation: the
    /// restore-point sweep now reads every document COLD. Real engine, real
    /// `document.write` ops, real capture — no mocks. Pins:
    ///  (a) capture hydrates NO rooms (registry residency stays empty on a
    ///      quiet cell — capture must never decode a Y.Doc into the registry);
    ///  (b) each captured Y.Doc bytes file equals the durable sidecar
    ///      byte-for-byte (cold reads did not degrade restore fidelity);
    ///  (c) a LEGACY inline-only record (sidecar removed) still captures its
    ///      real bytes through the bounded single-document fallback, which
    ///      backfills the sidecar via the ordinary hydrating read.
    #[test]
    fn capture_restore_point_reads_cold_and_hydrates_no_rooms() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-tt-capture-cold-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "tt-capture-cold";
            let doc_ids = ["cap-doc-a", "cap-doc-b", "cap-doc-legacy"];
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Capture Cold".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            // Real documents with real rewrite histories through the real
            // CRDT engine (three full rewrites each).
            for doc_id in doc_ids {
                for rewrite in 0..3 {
                    crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                        app.clone(),
                        EnqueueCrdtOperationInput {
                            kind: "document.write".to_string(),
                            graph_id: graph_id.to_string(),
                            document_id: Some(doc_id.to_string()),
                            payload: json!({
                                "documentId": doc_id,
                                "content": format!(
                                    "Rewrite {rewrite} of {doc_id}: real history weight."
                                ),
                                "format": "markdown",
                                "title": format!("Captured {doc_id}"),
                            }),
                        },
                    ))
                    .expect("document.write drains through the real engine");
                }
            }

            // The legacy shape: inline update in document.json, sidecar gone.
            let legacy_id = "cap-doc-legacy";
            let legacy_sidecar = document_ydoc_state_path(&graph_dir, legacy_id);
            assert!(legacy_sidecar.is_file(), "writes persist the sidecar");
            let legacy_bytes_before = std::fs::read(&legacy_sidecar).expect("legacy sidecar bytes");
            std::fs::remove_file(&legacy_sidecar).expect("simulate pre-sidecar legacy record");

            // Quiet cell: no rooms live (the interval scheduler's normal
            // world after a restart or between attaches).
            let registry = app.state::<crate::crdt_engine::rooms::RoomRegistry>();
            registry.evict_graph(graph_id);
            let doc_prefix = format!("doc:{graph_id}:");
            assert!(
                crate::app_runtime::async_runtime::block_on(registry.rooms_with_prefix(&doc_prefix)).is_empty()
            );

            let summary =
                capture_restore_point(&app, graph_id, RestorePointTrigger::Interval, None)
                    .expect("capture restore point");
            let restore_point_id = summary["restorePointId"]
                .as_str()
                .expect("restore point id")
                .to_string();

            // (a) NO rooms hydrated by the capture sweep.
            assert!(
                crate::app_runtime::async_runtime::block_on(registry.rooms_with_prefix(&doc_prefix)).is_empty(),
                "capture must never decode documents into the room registry"
            );
            assert!(
                crate::app_runtime::async_runtime::block_on(registry.peek(&format!("workspace:{graph_id}")))
                    .is_none(),
                "capture reads the workspace from its files, not a room"
            );

            // (b) Byte-for-byte Y.Doc fidelity for ordinary sidecar records.
            for doc_id in ["cap-doc-a", "cap-doc-b"] {
                let sidecar =
                    std::fs::read(document_ydoc_state_path(&graph_dir, doc_id)).expect("sidecar");
                let captured = crate::time_travel_store::read_document_bytes(
                    &graph_dir,
                    &restore_point_id,
                    doc_id,
                )
                .expect("captured bytes");
                assert_eq!(
                    captured, sidecar,
                    "captured Y.Doc bytes must equal the durable sidecar for {doc_id}"
                );
            }

            // (c) The legacy inline-only record: the bounded fallback
            // backfilled the sidecar and captured the REAL history bytes.
            let captured_legacy = crate::time_travel_store::read_document_bytes(
                &graph_dir,
                &restore_point_id,
                legacy_id,
            )
            .expect("captured legacy bytes");
            assert_eq!(
                captured_legacy, legacy_bytes_before,
                "legacy inline-only records keep full Y.Doc capture fidelity"
            );
            assert!(
                legacy_sidecar.is_file(),
                "the fallback runs the ordinary legacy backfill for that one document"
            );
            // …and the fallback stayed cold with respect to rooms too.
            assert!(
                crate::app_runtime::async_runtime::block_on(registry.rooms_with_prefix(&doc_prefix)).is_empty()
            );

            // FAIL CLOSED: when the legacy fallback cannot recover a
            // document's real update payload, the capture must FAIL — never
            // silently write an empty payload while the manifest advertises
            // a present ydoc_bytes_path (an accepted-but-empty restore point
            // is data loss at restore time). Corrupt one record into the
            // failing legacy shape: unreadable inline payload, no sidecar.
            let broken_id = "cap-doc-b";
            let broken_manifest = crate::paths::documents_dir(&graph_dir)
                .join(broken_id)
                .join("document.json");
            let mut manifest_value: serde_json::Value =
                crate::storage::read_json(&broken_manifest).expect("broken manifest json");
            manifest_value["ydocUpdateBase64"] = serde_json::json!("!!!not-base64!!!");
            crate::storage::write_json(&broken_manifest, &manifest_value)
                .expect("write corrupted manifest");
            std::fs::remove_file(document_ydoc_state_path(&graph_dir, broken_id))
                .expect("remove broken sidecar");

            let error = capture_restore_point(&app, graph_id, RestorePointTrigger::Interval, None)
                .expect_err("a failing legacy fallback must fail the capture");
            let message = error.to_string();
            assert!(
                message.contains("fallback"),
                "capture failure names the fallback seam: {message}"
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// The restored-record shape defeats the naive "empty inline = honest
    /// empty" check: `restore_document_metadata` deliberately clears the
    /// inline payload on EDITED documents. If such a document's sidecar later
    /// goes missing, capture must REFUSE rather than write an accepted-empty
    /// restore payload behind a present `ydoc_bytes_path` (silent history
    /// loss at restore time). Real engine, real `document.write` record
    /// reshaped exactly the way restore leaves it — no mocks.
    #[test]
    fn capture_refuses_empty_payload_for_edited_record_with_missing_sidecar() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-tt-capture-edited-empty-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "tt-capture-edited-empty";
            let doc_id = "edited-empty-doc";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Capture Edited Empty".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(doc_id.to_string()),
                    payload: json!({
                        "documentId": doc_id,
                        "content": "Edited content that must never capture as empty.",
                        "format": "markdown",
                        "title": "Edited Doc",
                    }),
                },
            ))
            .expect("document.write drains through the real engine");

            // Reshape into exactly what restore leaves behind: inline payload
            // cleared on an edited (revision >= 1, blocks present) record —
            // then lose the sidecar.
            let manifest_path = crate::paths::documents_dir(&graph_dir)
                .join(doc_id)
                .join("document.json");
            let mut manifest_value: serde_json::Value =
                crate::storage::read_json(&manifest_path).expect("manifest json");
            manifest_value["ydocUpdateBase64"] = serde_json::json!("");
            crate::storage::write_json(&manifest_path, &manifest_value)
                .expect("write restore-shaped manifest");
            std::fs::remove_file(document_ydoc_state_path(&graph_dir, doc_id))
                .expect("lose the sidecar");

            let error = capture_restore_point(&app, graph_id, RestorePointTrigger::Interval, None)
                .expect_err("an edited record with no recoverable Y.Doc state must fail capture");
            let message = error.to_string();
            assert!(
                message.contains("refusing to write an empty restore payload"),
                "capture refusal names the mechanism: {message}"
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// Companion to the refusal test: a legitimately-EMPTY document that was
    /// merely RENAMED (metadata-only writes bump `revision` unconditionally, so
    /// it carries `revision >= 1`) whose sidecar has gone missing and whose
    /// inline payload is empty must CAPTURE SUCCESSFULLY with an empty payload —
    /// not be rejected. The old `revision == 0` gate wrongly failed this case
    /// (no saved document ever has revision 0). Real engine, real
    /// `document.write`, record reshaped into the renamed-empty shape — no
    /// mocks: an empty-content record with `revision >= 1` and no sidecar
    /// captures a zero-byte Y.Doc payload and the capture returns a restore
    /// point.
    #[test]
    fn capture_accepts_empty_payload_for_renamed_never_edited_document() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-tt-capture-renamed-empty-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "tt-capture-renamed-empty";
            let doc_id = "renamed-empty-doc";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Capture Renamed Empty".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            // One real write materializes the document.json + workspace entry
            // + sidecar through the real engine.
            crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(doc_id.to_string()),
                    payload: json!({
                        "documentId": doc_id,
                        "content": "seed content that the rename later clears",
                        "format": "markdown",
                        "title": "Renamed Doc",
                    }),
                },
            ))
            .expect("document.write drains through the real engine");

            // Reshape into the renamed-but-never-content-edited shape: a
            // metadata-only rename bumped `revision` past 1, the inline payload
            // is empty, and EVERY content signal is empty — exactly a document
            // that legitimately has no state.
            let manifest_path = crate::paths::documents_dir(&graph_dir)
                .join(doc_id)
                .join("document.json");
            let mut manifest_value: serde_json::Value =
                crate::storage::read_json(&manifest_path).expect("manifest json");
            manifest_value["revision"] = serde_json::json!(4);
            manifest_value["ydocUpdateBase64"] = serde_json::json!("");
            manifest_value["body"] = serde_json::json!("");
            manifest_value["tiptapXml"] = serde_json::json!("");
            manifest_value["tiptapJson"] = serde_json::Value::Null;
            manifest_value["tree"] = serde_json::Value::Null;
            manifest_value["blocks"] = serde_json::json!([]);
            crate::storage::write_json(&manifest_path, &manifest_value)
                .expect("write renamed-empty manifest");
            // Lose the sidecar: the renamed empty document has no Y.Doc state.
            std::fs::remove_file(document_ydoc_state_path(&graph_dir, doc_id))
                .expect("lose the sidecar");

            let summary =
                capture_restore_point(&app, graph_id, RestorePointTrigger::Interval, None)
                    .expect("a renamed, content-empty document must capture successfully");
            let restore_point_id = summary["restorePointId"]
                .as_str()
                .expect("restore point id")
                .to_string();

            // The captured Y.Doc payload for the renamed-empty document is an
            // accepted zero-byte payload — honest empty, not silent loss.
            let captured = crate::time_travel_store::read_document_bytes(
                &graph_dir,
                &restore_point_id,
                doc_id,
            )
            .expect("captured bytes for renamed-empty document");
            assert!(
                captured.is_empty(),
                "a content-empty renamed document captures an empty Y.Doc payload"
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
