use crate::app_runtime::AppHandle;
pub(super) use crate::document_delete_service::delete_document;
pub(super) use crate::document_persistence_service::{
    save_document, save_document_ydoc_state, save_workspace, save_workspace_ydoc_state,
};
pub(super) use crate::document_record_store::{
    read_document_record, read_document_record_cold, read_graph_documents_cold,
    read_workspace_record, write_document_record,
};
pub(super) use crate::document_types::{
    BlockSnapshot, CreateDocumentInput, DocumentRecord, InlineMarkSnapshot, SaveDocumentInput,
    SaveDocumentYDocStateInput, SaveWorkspaceYDocStateInput, TreeNodeSnapshot, WorkspaceRecord,
};
#[cfg(test)]
pub(super) use crate::document_types::{DocumentTreeSnapshot, TreeNodeAttributes};
use crate::{
    clock::timestamp,
    document_persistence_service::ensure_document_persistence_tail,
    document_sidecar_store::empty_ydoc_update_base64,
    ids::{make_document_id, normalize_title, validate_local_id},
    paths::{
        document_dir, document_ydoc_state_path, documents_dir, ensure_document_dirs,
        existing_document_dir, existing_graph_dir,
    },
    rdf::document_subject,
    restore_guard::require_no_active_restore,
    runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    storage::{create_dir_all, display_path},
};
use std::{fs, path::PathBuf};

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn list_documents(
    app: AppHandle,
    graph_id: String,
) -> Result<Vec<DocumentRecord>, String> {
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let documents_dir = documents_dir(&graph_dir);
    create_dir_all(&documents_dir)?;

    let mut documents = Vec::new();
    let entries =
        fs::read_dir(&documents_dir).map_err(|error| format!("read documents dir: {error}"))?;

    for entry in entries {
        let entry = entry.map_err(|error| format!("read document entry: {error}"))?;
        let manifest_path = entry.path().join("document.json");
        if !manifest_path.is_file() {
            continue;
        }
        let Some(document_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if crate::document_tombstone_store::document_is_tombstoned(&graph_dir, &document_id)? {
            continue;
        }
        documents.push(read_document_record(&graph_dir, &manifest_path)?);
    }

    documents.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(documents)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn create_document(
    app: AppHandle,
    input: CreateDocumentInput,
) -> Result<DocumentRecord, String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            &app,
            &input.graph_id,
        )?;
    create_document_with_lease(app, input)
}

/// Non-reentrant create body for queue/import handlers that already hold the
/// graph persistence lease.
pub(crate) fn create_document_with_lease(
    app: AppHandle,
    input: CreateDocumentInput,
) -> Result<DocumentRecord, String> {
    let title = normalize_title(&input.title)?;
    create_document_record_with_lease(app, input, title)
}

/// Archive-only entry: exact bounded stored title, explicit destination ID.
/// This changes no authority/deletion/existence fences or public create policy.
pub(crate) fn create_imported_document_with_lease(
    app: AppHandle,
    input: CreateDocumentInput,
) -> Result<DocumentRecord, String> {
    let title = crate::ids::normalize_stored_title(&input.title)?;
    if title != input.title || input.document_id.is_none() {
        return Err("imported document requires exact title and explicit identity".into());
    }
    create_document_record_with_lease(app, input, title)
}

fn create_document_record_with_lease(
    app: AppHandle,
    input: CreateDocumentInput,
    title: String,
) -> Result<DocumentRecord, String> {
    let _durability_guard = crate::cell_durability::write_guard();
    require_no_active_restore(&app, &input.graph_id)?;
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    let document_id = match input.document_id {
        Some(document_id) => {
            validate_local_id(&document_id, "document_id")?;
            document_id
        }
        None => make_document_id(&title),
    };
    let document_dir = document_dir(&graph_dir, &document_id)?;
    let now = timestamp();
    crate::document_body_availability::require_available(&graph_dir, &document_id)?;

    if crate::document_tombstone_store::document_is_tombstoned(&graph_dir, &document_id)? {
        return Err(format!(
            "document tombstoned: {document_id}; recreate it through document.write"
        ));
    }

    if document_dir.join("document.json").is_file() {
        let document = read_document_record(&graph_dir, &document_dir.join("document.json"))?;
        ensure_document_persistence_tail(&graph_dir, &document)?;
        return Ok(document);
    }

    ensure_document_dirs(&graph_dir, &document_id)?;

    let ydoc_state_path = display_path(&document_ydoc_state_path(&graph_dir, &document_id));
    let document = DocumentRecord {
        document_id: document_id.clone(),
        graph_id: input.graph_id,
        title,
        revision: 0,
        body: String::new(),
        origin: LOCAL_GRAPH_ORIGIN.to_string(),
        provider_id: LOCAL_PROVIDER_ID.to_string(),
        local_path: display_path(&document_dir),
        rdf_subject: document_subject(&document_id),
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
        ydoc_update_base64: empty_ydoc_update_base64(),
        ydoc_state_path,
        tree: None,
        blocks: Vec::new(),
        rdf_triple_count: 0,
        document_kind: None,
    };

    write_document_record(&graph_dir, &document)?;
    ensure_document_persistence_tail(&graph_dir, &document)?;

    Ok(document)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn read_document(
    app: AppHandle,
    graph_id: String,
    document_id: String,
) -> Result<DocumentRecord, String> {
    let (graph_dir, document_dir) = document_read_paths(&app, &graph_id, &document_id, false)?;
    read_document_record(&graph_dir, &document_dir.join("document.json"))
}

/// Hydrating read for a caller that already owns the graph persistence lease.
///
/// The distinction matters only on headless ghost self-heal: the ordinary
/// authoring read acquires the graph lease before healing, while this variant
/// must call the non-reentrant healing body.
pub(crate) fn read_document_with_lease(
    app: AppHandle,
    graph_id: String,
    document_id: String,
) -> Result<DocumentRecord, String> {
    let (graph_dir, document_dir) = document_read_paths(&app, &graph_id, &document_id, true)?;
    read_document_record(&graph_dir, &document_dir.join("document.json"))
}

/// COLD single-document read: identical route checks, tombstone fence, and
/// headless ghost self-heal as [`read_document`], but the record comes back
/// via `read_document_record_cold` — content projection only, NO Y.Doc
/// history hydration. This is the read for consumers that never decode or
/// forward the update payload (document-history snapshot capture, and any
/// future projection-only reader); on fat documents the hydrating read costs
/// O(full rewrite history) per call for bytes those callers immediately drop.
pub(super) fn read_document_cold(
    app: AppHandle,
    graph_id: String,
    document_id: String,
) -> Result<DocumentRecord, String> {
    read_document_cold_inner(app, graph_id, document_id, false)
}

/// Cold read for a caller that already owns the graph persistence lease.
pub(crate) fn read_document_cold_with_lease(
    app: AppHandle,
    graph_id: String,
    document_id: String,
) -> Result<DocumentRecord, String> {
    read_document_cold_inner(app, graph_id, document_id, true)
}

fn read_document_cold_inner(
    app: AppHandle,
    graph_id: String,
    document_id: String,
    graph_lease_held: bool,
) -> Result<DocumentRecord, String> {
    let (graph_dir, document_dir) =
        document_read_paths(&app, &graph_id, &document_id, graph_lease_held)?;
    read_document_record_cold(&graph_dir, &document_dir.join("document.json"))
}

fn document_read_paths(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    graph_lease_held: bool,
) -> Result<(PathBuf, PathBuf), String> {
    #[cfg(any(not(feature = "headless"), feature = "desktop"))]
    let _ = graph_lease_held;
    let graph_dir = existing_graph_dir(app, graph_id)?;
    crate::document_tombstone_store::require_document_not_tombstoned(&graph_dir, document_id)?;
    let document_dir = document_dir(&graph_dir, document_id)?;
    if !document_dir.join("document.json").is_file() {
        // F8: a genuine ghost (still listed in the workspace Y.Doc but
        // missing its manifest) self-heals only in a pure headless cell with
        // self-heal enabled. Select the non-reentrant body when the caller
        // already owns the graph persistence lease.
        #[cfg(all(feature = "headless", not(feature = "desktop")))]
        if graph_lease_held {
            if !crate::runtime_config::self_heal_graphs_enabled() {
                return Err(format!("document not found: {document_id}"));
            }
            crate::document_paths::self_heal_missing_document_with_lease(
                app,
                &graph_dir,
                graph_id,
                document_id,
            )?;
        } else {
            crate::document_paths::self_heal_missing_document(
                app,
                &graph_dir,
                graph_id,
                document_id,
            )?;
        }
        #[cfg(any(not(feature = "headless"), feature = "desktop"))]
        return Err(format!("document not found: {document_id}"));
    }
    let document_dir = existing_document_dir(&graph_dir, document_id)?;
    Ok((graph_dir, document_dir))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn read_workspace(app: AppHandle, graph_id: String) -> Result<WorkspaceRecord, String> {
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    read_workspace_record(&graph_dir, &graph_id)
}
