use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    crdt_queue::record_crdt_phase,
    document_history_file_store::{read_document_tail_commit, write_document_tail_commit},
    document_history_service::{
        committed_snapshot_for_document, ensure_document_snapshot_for_revision,
        ensure_document_snapshot_for_revision_with_tombstone_fence,
    },
    document_history_store::{LocalDocumentTailCommit, DOCUMENT_TAIL_COMMIT_SCHEMA_VERSION},
    document_meaningful_object::reconcile_document_record,
    document_record_store::{read_workspace_record, write_document_record, write_ydoc_update},
    document_service::{
        DocumentRecord, SaveDocumentInput, SaveDocumentYDocStateInput, SaveWorkspaceYDocStateInput,
    },
    document_types::{SaveWorkspaceInput, WorkspaceRecord},
    graph_record_store::{touch_graph_content_revision, GraphRecord},
    ids::normalize_stored_title,
    paths::{
        document_dir, document_ydoc_state_path, ensure_document_dirs, existing_document_dir,
        existing_graph_dir, workspace_snapshot_path, workspace_ydoc_state_path,
    },
    rdf::document_subject,
    rdf_service::{document_tree_triples, open_graph_store, reconcile_workspace_snapshot},
    restore_guard::require_no_active_restore,
    runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    storage::{display_path, read_json, write_json},
};
use std::{path::Path, time::Instant};

fn document_tail_commit_is_current(
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<bool, String> {
    // A corrupt marker is incomplete tail state, not authority. The repair
    // path overwrites it atomically after rebuilding every preceding stage.
    let commit = match read_document_tail_commit(graph_dir, &document.document_id) {
        Ok(Some(commit)) => commit,
        Ok(None) | Err(_) => return Ok(false),
    };
    if commit.schema_version != DOCUMENT_TAIL_COMMIT_SCHEMA_VERSION
        || commit.graph_id != document.graph_id
        || commit.document_id != document.document_id
        || commit.document_revision != document.revision
    {
        return Ok(false);
    }
    let graph = read_json::<GraphRecord>(&graph_dir.join("graph.json"))?;
    if graph.graph_id != document.graph_id {
        return Ok(false);
    }
    let graph_covers_commit = match (
        graph
            .content_revision
            .as_deref()
            .and_then(crate::clock::parse_timestamp),
        crate::clock::parse_timestamp(&commit.graph_content_revision),
    ) {
        (Some(current), Some(committed)) => current >= committed,
        _ => graph.content_revision.as_deref() == Some(commit.graph_content_revision.as_str()),
    };
    if !graph_covers_commit {
        return Ok(false);
    }
    Ok(committed_snapshot_for_document(graph_dir, document, &commit.snapshot_id)?.is_some())
}

/// Complete or repair every durable projection for one exact document
/// revision. The final marker is authoritative only after all preceding
/// stages succeed; this function never writes document.json or increments its
/// revision.
pub(super) fn ensure_document_persistence_tail(
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<bool, String> {
    ensure_document_persistence_tail_with_tombstone_fence(graph_dir, document, None)
}

pub(crate) fn ensure_document_persistence_tail_with_tombstone_fence(
    graph_dir: &Path,
    document: &DocumentRecord,
    expected_deletion_id: Option<&str>,
) -> Result<bool, String> {
    let _durability_guard = crate::cell_durability::write_guard();
    if let Some(expected_deletion_id) = expected_deletion_id {
        crate::document_tombstone_store::require_document_tombstone_matches(
            graph_dir,
            &document.document_id,
            expected_deletion_id,
        )?;
    }
    if document_tail_commit_is_current(graph_dir, document)? {
        return Ok(false);
    }

    let store = open_graph_store(graph_dir)?;
    reconcile_document_record(&store, document)?;
    // Flow boards additionally reconcile the graph's `:projection:flow` lane
    // from the record's Y.Doc bytes (a no-op for every other kind) — same
    // repairable tail slot as the document MO reconcile above, so a crashed
    // tail replays it under the same revision. Unit G3.
    crate::flow_board_reconcile::reconcile_flow_board_record(&store, graph_dir, document)?;
    crate::pdf_source::reconcile(&store, document)?;
    let snapshot = if let Some(expected_deletion_id) = expected_deletion_id {
        ensure_document_snapshot_for_revision_with_tombstone_fence(
            graph_dir,
            document,
            Some(expected_deletion_id),
        )?
    } else {
        ensure_document_snapshot_for_revision(graph_dir, document)?
    };
    let graph_content_revision =
        touch_graph_content_revision(graph_dir).map_err(crate::app_error::AppError::message)?;
    write_document_tail_commit(
        graph_dir,
        &LocalDocumentTailCommit {
            schema_version: DOCUMENT_TAIL_COMMIT_SCHEMA_VERSION,
            graph_id: document.graph_id.clone(),
            document_id: document.document_id.clone(),
            document_revision: document.revision,
            snapshot_id: snapshot.snapshot_id,
            graph_content_revision,
        },
    )?;
    Ok(true)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn save_workspace(
    app: AppHandle,
    input: SaveWorkspaceInput,
) -> Result<WorkspaceRecord, String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            &app,
            &input.graph_id,
        )?;
    save_workspace_with_lease(app, input)
}

/// Non-reentrant workspace save body for a queue/import root that already
/// owns graph persistence authority.
pub(crate) fn save_workspace_with_lease(
    app: AppHandle,
    input: SaveWorkspaceInput,
) -> Result<WorkspaceRecord, String> {
    let _flush_guard = crate::cell_durability::write_guard();
    require_no_active_restore(&app, &input.graph_id)?;
    let trace_operation_id = input.trace_operation_id.clone();
    let load_started = Instant::now();
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustWorkspaceLoadGraphMs",
        load_started.elapsed(),
    );

    let write_ydoc_started = Instant::now();
    write_ydoc_update(
        &workspace_ydoc_state_path(&graph_dir),
        &input.ydoc_update_base64,
    )?;
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustWriteWorkspaceYdocMs",
        write_ydoc_started.elapsed(),
    );
    if let Some(snapshot) = input.snapshot {
        let write_snapshot_started = Instant::now();
        write_json(&workspace_snapshot_path(&graph_dir), &snapshot)?;
        record_crdt_phase(
            &app,
            trace_operation_id.as_deref(),
            "rustWriteWorkspaceSnapshotMs",
            write_snapshot_started.elapsed(),
        );
        let materialize_started = Instant::now();
        let store = open_graph_store(&graph_dir)?;
        reconcile_workspace_snapshot(&store, &input.graph_id, &snapshot)?;
        record_crdt_phase(
            &app,
            trace_operation_id.as_deref(),
            "rustMaterializeWorkspaceRdfMs",
            materialize_started.elapsed(),
        );
    }
    let touch_started = Instant::now();
    touch_graph_content_revision(&graph_dir)?;
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustTouchWorkspaceGraphMs",
        touch_started.elapsed(),
    );
    let read_started = Instant::now();
    let record = read_workspace_record(&graph_dir, &input.graph_id);
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustReadWorkspaceRecordMs",
        read_started.elapsed(),
    );
    record
}

/// Hot-path workspace persistence: write only the workspace Y.Doc binary state
/// for crash recovery. Full workspace snapshot/RDF materialization is handled
/// by the debounced cold flush.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn save_workspace_ydoc_state(
    app: AppHandle,
    input: SaveWorkspaceYDocStateInput,
) -> Result<(), String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            &app,
            &input.graph_id,
        )?;
    save_workspace_ydoc_state_with_lease(app, input)
}

fn save_workspace_ydoc_state_with_lease(
    app: AppHandle,
    input: SaveWorkspaceYDocStateInput,
) -> Result<(), String> {
    let _flush_guard = crate::cell_durability::write_guard();
    require_no_active_restore(&app, &input.graph_id)?;
    let trace_operation_id = input.trace_operation_id.clone();
    let started = Instant::now();
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    write_ydoc_update(
        &workspace_ydoc_state_path(&graph_dir),
        &input.ydoc_update_base64,
    )?;
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustWriteWorkspaceYDocStateMs",
        started.elapsed(),
    );
    Ok(())
}

/// Hot-path persistence: write only the Y.Doc binary state for crash recovery.
/// Skips document.json updates, projection files, RDF re-materialization, and
/// the graph touch between debounced full materializations.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn save_document_ydoc_state(
    app: AppHandle,
    input: SaveDocumentYDocStateInput,
) -> Result<(), String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            &app,
            &input.graph_id,
        )?;
    save_document_ydoc_state_with_lease(app, input)
}

fn save_document_ydoc_state_with_lease(
    app: AppHandle,
    input: SaveDocumentYDocStateInput,
) -> Result<(), String> {
    let _flush_guard = crate::cell_durability::write_guard();
    require_no_active_restore(&app, &input.graph_id)?;
    let trace_operation_id = input.trace_operation_id.clone();
    let started = Instant::now();
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    crate::document_tombstone_store::require_document_not_tombstoned(
        &graph_dir,
        &input.document_id,
    )?;
    let _ = existing_document_dir(&graph_dir, &input.document_id)?;
    write_ydoc_update(
        &document_ydoc_state_path(&graph_dir, &input.document_id),
        &input.ydoc_update_base64,
    )?;
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustWriteYDocStateMs",
        started.elapsed(),
    );
    Ok(())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn save_document(
    app: AppHandle,
    input: SaveDocumentInput,
) -> Result<DocumentRecord, String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            &app,
            &input.graph_id,
        )?;
    save_document_with_lease(app, input)
}

/// Non-reentrant document save body for queue/import handlers that already
/// hold the graph persistence lease.
pub(crate) fn save_document_with_lease(
    app: AppHandle,
    input: SaveDocumentInput,
) -> Result<DocumentRecord, String> {
    save_document_with_tombstone_fence(app, input, None)
}

/// Persist a fresh same-ID authority while retaining its public deletion
/// marker. The exact deletion UUID is revalidated by the history tail before
/// the save can commit as a legitimate recreation.
pub(crate) fn save_document_for_recreation(
    app: AppHandle,
    input: SaveDocumentInput,
    expected_deletion_id: &str,
) -> Result<DocumentRecord, String> {
    save_document_with_tombstone_fence(app, input, Some(expected_deletion_id))
}

fn save_document_with_tombstone_fence(
    app: AppHandle,
    input: SaveDocumentInput,
    expected_deletion_id: Option<&str>,
) -> Result<DocumentRecord, String> {
    let _flush_guard = crate::cell_durability::write_guard();
    require_no_active_restore(&app, &input.graph_id)?;
    let trace_operation_id = input.trace_operation_id.clone();
    let load_started = Instant::now();
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    if let Some(expected_deletion_id) = expected_deletion_id {
        crate::document_tombstone_store::require_document_tombstone_matches(
            &graph_dir,
            &input.document_id,
            expected_deletion_id,
        )?;
    } else {
        crate::document_tombstone_store::require_document_not_tombstoned(
            &graph_dir,
            &input.document_id,
        )?;
    }
    let title = normalize_stored_title(&input.title)?;
    let mut document = read_or_initialize_document_record(
        &graph_dir,
        &input.graph_id,
        &input.document_id,
        &title,
    )?;
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustLoadDocumentRecordMs",
        load_started.elapsed(),
    );
    document.title = title;
    document.revision = next_revision(document.revision, input.expected_revision)?;
    document.body = input.body;
    document.schema_version = DOCUMENT_SCHEMA_VERSION;
    document.tiptap_xml = input.tiptap_xml.unwrap_or_default();
    document.tiptap_json = input.tiptap_json;
    // Sticky kind (unit G3): a kind is declared at creation and never changes
    // (interfaces.md §A), so a save that carries one records it and a
    // metadata-only save that omits it must not erase it.
    if let Some(kind) = input.document_kind {
        document.document_kind = Some(kind);
    }
    document.ydoc_update_base64 = input.ydoc_update_base64.unwrap_or_default();
    document.ydoc_state_path =
        display_path(&document_ydoc_state_path(&graph_dir, &document.document_id));
    document.tree = input.tree;
    document.blocks = input.blocks.unwrap_or_default();
    let tree_triples = document_tree_triples(&document);
    document.rdf_triple_count = tree_triples.len();
    document.updated_at = timestamp();

    let write_started = Instant::now();
    write_document_record(&graph_dir, &document)?;
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustWriteDocumentFilesMs",
        write_started.elapsed(),
    );
    let materialize_started = Instant::now();
    ensure_document_persistence_tail_with_tombstone_fence(
        &graph_dir,
        &document,
        expected_deletion_id,
    )?;
    record_crdt_phase(
        &app,
        trace_operation_id.as_deref(),
        "rustPersistDocumentTailMs",
        materialize_started.elapsed(),
    );

    Ok(document)
}

/// A2 item 9 — idempotency-aware revision increment.
///
/// Returns the next revision value or an error on unrelated mismatch.
///
/// | `expected_revision` | `current` vs `expected` | outcome                          |
/// |---------------------|-------------------------|----------------------------------|
/// | `None`              | any                     | `current + 1` (unconditional)    |
/// | `Some(e)`           | `current == e`          | `current + 1` (normal write)     |
/// | `Some(e)`           | `current == e + 1`      | `current` (already-applied replay) |
/// | `Some(e)`           | anything else           | `Err` (conflict)                 |
pub(crate) fn next_revision(current: u64, expected_revision: Option<u64>) -> Result<u64, String> {
    match expected_revision {
        None => Ok(current.saturating_add(1)),
        Some(expected) if current == expected => Ok(current.saturating_add(1)),
        Some(expected) if current == expected.saturating_add(1) => Ok(current),
        Some(expected) => Err(format!(
            "revision conflict: expected {expected}, actual {current}"
        )),
    }
}

fn read_or_initialize_document_record(
    graph_dir: &Path,
    graph_id: &str,
    document_id: &str,
    title: &str,
) -> Result<DocumentRecord, String> {
    let document_dir = document_dir(graph_dir, document_id)?;
    if document_dir.join("document.json").is_file() {
        return read_json::<DocumentRecord>(&document_dir.join("document.json"))
            .map_err(Into::into);
    }

    ensure_document_dirs(graph_dir, document_id)?;
    let now = timestamp();
    Ok(DocumentRecord {
        document_id: document_id.to_string(),
        graph_id: graph_id.to_string(),
        title: title.to_string(),
        revision: 0,
        body: String::new(),
        origin: LOCAL_GRAPH_ORIGIN.to_string(),
        provider_id: LOCAL_PROVIDER_ID.to_string(),
        local_path: display_path(&document_dir),
        rdf_subject: document_subject(document_id),
        created_at: now.clone(),
        updated_at: now,
        capabilities: vec![
            "document.local.read".to_string(),
            "document.local.write".to_string(),
            "document.local.materialize.rdf".to_string(),
            "document.local.ydoc".to_string(),
            "document.local.tiptap-tree".to_string(),
        ],
        schema_version: DOCUMENT_SCHEMA_VERSION,
        tiptap_xml: String::new(),
        tiptap_json: None,
        ydoc_update_base64: String::new(),
        ydoc_state_path: display_path(&document_ydoc_state_path(graph_dir, document_id)),
        tree: None,
        blocks: Vec::new(),
        rdf_triple_count: 0,
        document_kind: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn temp_graph_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()))
    }

    #[test]
    fn read_or_initialize_document_record_bootstraps_missing_manifest() {
        let graph_dir = temp_graph_dir("sophia-save-document-bootstrap");
        let document_id = "doc-bootstrap";

        let record =
            read_or_initialize_document_record(&graph_dir, "graph-a", document_id, "Draft")
                .expect("bootstrap document record");

        assert_eq!(record.document_id, document_id);
        assert_eq!(record.graph_id, "graph-a");
        assert_eq!(record.title, "Draft");
        assert_eq!(record.revision, 0);
        assert!(graph_dir.join("documents/doc-bootstrap").is_dir());
        assert!(graph_dir.join("ydocs/documents/doc-bootstrap").is_dir());
        fs::remove_dir_all(graph_dir).unwrap();
    }

    // --- A2 item 9: next_revision tests ---

    /// Normal write: expected_revision matches current → increment.
    #[test]
    fn revision_increments_when_expected_revision_matches() {
        assert_eq!(next_revision(3, Some(3)).unwrap(), 4);
        // Also works at 0 (new document).
        assert_eq!(next_revision(0, Some(0)).unwrap(), 1);
    }

    /// Replay: current is already ahead-by-one → treat as already-applied,
    /// return current unchanged.
    #[test]
    fn revision_skips_increment_on_replay_when_expected_plus_one() {
        assert_eq!(next_revision(4, Some(3)).unwrap(), 4);
        assert_eq!(next_revision(1, Some(0)).unwrap(), 1);
    }

    /// Unrelated mismatch (current differs by more than one from expected) →
    /// conflict error.
    #[test]
    fn revision_returns_conflict_on_unrelated_mismatch() {
        let err = next_revision(5, Some(3)).unwrap_err();
        assert!(
            err.contains("conflict"),
            "expected conflict in error message, got: {err}"
        );
        // Also errors when current is behind expected.
        let err2 = next_revision(2, Some(5)).unwrap_err();
        assert!(err2.contains("conflict"), "expected conflict, got: {err2}");
    }

    /// Backward-compat: absent expected_revision → unconditional increment.
    #[test]
    fn revision_increments_unconditionally_when_expected_revision_absent() {
        assert_eq!(next_revision(0, None).unwrap(), 1);
        assert_eq!(next_revision(99, None).unwrap(), 100);
        // Saturating at u64::MAX.
        assert_eq!(next_revision(u64::MAX, None).unwrap(), u64::MAX);
    }
}
