use crate::{
    document_history_store::{DOCUMENT_TAIL_COMMIT_FILE, HISTORY_STORE_FILE},
    graph_record_store::GraphRecord,
    original_file_manifest_store::{original_manifest_file_path, read_original_manifest},
    paths::{
        artifacts_dir, document_ydoc_state_path, documents_dir, ensure_graph_index_dirs,
        images_dir, workspace_snapshot_path, workspace_ydoc_state_path,
    },
    rdf_store_service::evict_graph_store,
    runtime_config::GRAPH_STATUS_DELETED,
    storage::{
        copy_file_atomic, create_dir_all, display_path, read_bytes, read_json, remove_dir_all,
        write_bytes, write_json,
    },
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use fs4::fs_std::FileExt;
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Cross-entrypoint publication reservation for one graph ID.
///
/// This is deliberately separate from the CRDT lifecycle coordinator: archive
/// import already owns that coordinator lease when it creates its target. The
/// kernel file lock serializes desktop, MCP, hosted, self-heal, and duplicate
/// publishers without making those nested paths re-enter the CRDT lease.
pub(crate) struct GraphPublicationReservation {
    _lock: File,
    stage_dir: PathBuf,
    target_dir: PathBuf,
    state: Cell<GraphPublicationState>,
    adopted_unpublished_target: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingTargetPolicy {
    Reject,
    AdoptUnpublishedLayout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphPublicationState {
    Staged,
    Published,
    CleanupAttempted,
}

impl GraphPublicationReservation {
    pub(crate) fn acquire(
        profile_dir: &Path,
        target_dir: PathBuf,
        graph_id: &str,
        existing_target_policy: ExistingTargetPolicy,
    ) -> Result<Self, String> {
        let staging_root = profile_dir.join(".graph-staging");
        let lock = acquire_graph_publication_lock(profile_dir, graph_id)?;

        let hidden_stage_dir = staging_root.join(format!("{graph_id}-{}", Uuid::new_v4().simple()));
        let (stage_dir, adopted_unpublished_target) = if target_dir.exists() {
            if existing_target_policy != ExistingTargetPolicy::AdoptUnpublishedLayout
                || !is_adoptable_unpublished_graph_dir(&target_dir)?
            {
                return Err(format!("graph already exists: {graph_id}"));
            }
            // A pre-manifest layout may already contain the only durable copy
            // of a websocket-written Y.Doc. Keep that hot authority at its
            // canonical path while filling in the remaining layout. The final
            // atomic graph.json write is this mode's publication boundary.
            // Moving the directory to `.graph-staging` would create a crash
            // window in which neither startup nor a retry can discover it.
            remove_stale_graph_manifest_temps(&target_dir)?;
            (target_dir.clone(), true)
        } else {
            fs::create_dir(&hidden_stage_dir).map_err(|error| {
                format!(
                    "create graph publication stage {}: {error}",
                    display_path(&hidden_stage_dir)
                )
            })?;
            (hidden_stage_dir, false)
        };
        Ok(Self {
            _lock: lock,
            stage_dir,
            target_dir,
            state: Cell::new(GraphPublicationState::Staged),
            adopted_unpublished_target,
        })
    }

    pub(crate) fn stage_dir(&self) -> &Path {
        &self.stage_dir
    }

    /// Publish an active canonical record after every other essential graph
    /// file is ready. New graphs write the record in their hidden stage before
    /// the atomic directory rename. Adopted pre-manifest layouts stay in place
    /// and become visible only through the atomic graph.json replacement.
    pub(crate) fn publish_graph_record(&self, graph: &GraphRecord) -> Result<(), String> {
        if self.adopted_unpublished_target {
            let manifest_path = self.target_dir.join("graph.json");
            if manifest_path.exists() {
                return Err("target graph manifest already exists".to_string());
            }
            // The staging path and final path are identical in adoption mode.
            // Eviction ensures a handle opened while completing the layout is
            // not later mistaken for a different graph incarnation.
            evict_graph_store(&self.target_dir)?;
            if let Err(error) = write_json(&manifest_path, graph) {
                let expected = serde_json::to_value(graph).map_err(|serialize_error| {
                    format!("serialize expected graph: {serialize_error}")
                })?;
                return self.reconcile_ambiguous_publication_error(&expected, error.message());
            }
            self.state.set(GraphPublicationState::Published);
            return Ok(());
        }

        write_json(&self.stage_dir.join("graph.json"), graph)?;
        self.publish()
    }

    /// Publish the fully staged graph as one directory entry change. Every
    /// in-process publisher holds this reservation, so the existence check and
    /// rename form a no-clobber critical section even on platforms where a raw
    /// directory rename could replace an empty destination directory.
    pub(crate) fn publish(&self) -> Result<(), String> {
        if self.adopted_unpublished_target {
            return Err(
                "adopted graph layouts must publish their canonical record directly".to_string(),
            );
        }
        if self.target_dir.exists() {
            return Err("target graph directory already exists".to_string());
        }
        // The final path may still key an Arc to a physically removed earlier
        // incarnation. Remove it before the rename makes this incarnation
        // visible, even though the path does not currently exist on disk.
        evict_graph_store(&self.target_dir)?;
        let expected_manifest = read_json::<serde_json::Value>(&self.stage_dir.join("graph.json"))?;
        if let Err(error) = fs::rename(&self.stage_dir, &self.target_dir) {
            return self.reconcile_ambiguous_publication_error(
                &expected_manifest,
                format!(
                    "publish graph stage {} -> {}: {error}",
                    display_path(&self.stage_dir),
                    display_path(&self.target_dir)
                ),
            );
        }
        self.state.set(GraphPublicationState::Published);
        Ok(())
    }

    /// NFS/EFS may report a rename/write error after the server committed it.
    /// Treat the target's exact expected manifest as the authoritative answer;
    /// otherwise the caller could report failure while a live graph is already
    /// visible and then run pre-publication cleanup against the wrong path.
    fn reconcile_ambiguous_publication_error(
        &self,
        expected_manifest: &serde_json::Value,
        original_error: String,
    ) -> Result<(), String> {
        let source_no_longer_staged = self.adopted_unpublished_target || !self.stage_dir.exists();
        let target_matches = read_json::<serde_json::Value>(&self.target_dir.join("graph.json"))
            .map(|actual| actual == *expected_manifest)
            .unwrap_or(false);
        if source_no_longer_staged && target_matches {
            self.state.set(GraphPublicationState::Published);
            log::warn!(
                "Graph publication reported an error after the expected target manifest became authoritative; treating it as committed: {original_error}"
            );
            Ok(())
        } else {
            Err(original_error)
        }
    }

    pub(crate) fn sync_publication_parents_best_effort(&self) {
        for parent in [self.stage_dir.parent(), self.target_dir.parent()]
            .into_iter()
            .flatten()
        {
            if let Err(error) = crate::storage_atomic::sync_parent_dir(parent) {
                log::warn!(
                    "Failed to sync graph publication parent {}: {error}",
                    display_path(parent)
                );
            }
        }
    }

    /// Remove canonical active membership before attempting recursive cleanup.
    /// If recursive removal itself fails, an abandoned stage remains invisible
    /// to graph scans and contains no active canonical manifest.
    pub(crate) fn cleanup_failed_stage(&self, inject_remove_failure: bool) -> Vec<String> {
        self.state.set(GraphPublicationState::CleanupAttempted);
        let mut errors = Vec::new();
        let manifest_path = self.stage_dir.join("graph.json");
        if manifest_path.is_file() {
            let tombstone_result =
                read_json::<GraphRecord>(&manifest_path).and_then(|mut graph| {
                    graph.status = GRAPH_STATUS_DELETED.to_string();
                    graph.updated_at = crate::clock::timestamp();
                    write_json(&manifest_path, &graph)
                });
            if let Err(tombstone_error) = tombstone_result {
                if let Err(remove_error) = fs::remove_file(&manifest_path) {
                    errors.push(format!(
                        "neutralize failed staged graph manifest {}: {tombstone_error}; remove fallback: {remove_error}",
                        display_path(&manifest_path)
                    ));
                }
            }
        }
        if let Err(error) = evict_graph_store(&self.stage_dir) {
            errors.push(format!("evict staged graph RDF store: {error}"));
        }
        if self.adopted_unpublished_target {
            // Adoption never moves the pre-manifest directory. Failed create
            // leaves the hot sidecars discoverable at their canonical path and
            // removes only a manifest that did not reach committed publish.
            if manifest_path.exists() {
                if let Err(error) = fs::remove_file(&manifest_path) {
                    errors.push(format!(
                        "remove manifest from unpublished layout {}: {error}",
                        display_path(&manifest_path)
                    ));
                }
            }
            return errors;
        }
        if inject_remove_failure {
            errors.push("injected staged directory removal failure".to_string());
            return errors;
        }
        if self.stage_dir.exists() {
            if let Err(error) = remove_dir_all(&self.stage_dir) {
                errors.push(format!(
                    "remove graph publication stage {}: {error}",
                    display_path(&self.stage_dir)
                ));
            }
        }
        errors
    }
}

/// Kernel-backed lifecycle reservation shared by create, duplicate, and hard
/// delete. Keeping the returned file alive holds the exclusive lock.
pub(crate) fn acquire_graph_publication_lock(
    profile_dir: &Path,
    graph_id: &str,
) -> Result<File, String> {
    let locks_dir = profile_dir.join(".graph-staging").join("locks");
    create_dir_all(&locks_dir)?;
    let lock_path = locks_dir.join(format!("{graph_id}.lock"));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| {
            format!(
                "open graph publication lock {}: {error}",
                display_path(&lock_path)
            )
        })?;
    lock.lock_exclusive().map_err(|error| {
        format!(
            "acquire graph publication lock {}: {error}",
            display_path(&lock_path)
        )
    })?;
    Ok(lock)
}

fn is_adoptable_unpublished_graph_dir(target_dir: &Path) -> Result<bool, String> {
    if !target_dir.is_dir() || target_dir.join("graph.json").exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(target_dir).map_err(|error| {
        format!(
            "inspect unpublished graph layout {}: {error}",
            display_path(target_dir)
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "inspect unpublished graph layout entry {}: {error}",
                display_path(target_dir)
            )
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            return Ok(false);
        };
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect {} type: {error}", display_path(&entry.path())))?;
        if file_type.is_file() && is_graph_manifest_temp_name(&name) {
            continue;
        }
        if !file_type.is_dir() {
            return Ok(false);
        }
        if !matches!(
            name.as_str(),
            "ydocs" | "documents" | "artifacts" | "images" | "indexes" | "store.oxigraph"
        ) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn is_graph_manifest_temp_name(name: &str) -> bool {
    name.starts_with(".graph.json.tmp-")
}

fn remove_stale_graph_manifest_temps(target_dir: &Path) -> Result<(), String> {
    for entry in fs::read_dir(target_dir).map_err(|error| {
        format!(
            "inspect unpublished graph layout {}: {error}",
            display_path(target_dir)
        )
    })? {
        let entry = entry.map_err(|error| format!("read graph manifest temp entry: {error}"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if is_graph_manifest_temp_name(&name)
            && entry
                .file_type()
                .map_err(|error| format!("inspect {} type: {error}", display_path(&entry.path())))?
                .is_file()
        {
            fs::remove_file(entry.path()).map_err(|error| {
                format!(
                    "remove stale graph manifest temp {}: {error}",
                    display_path(&entry.path())
                )
            })?;
        }
    }
    Ok(())
}

impl Drop for GraphPublicationReservation {
    fn drop(&mut self) {
        // A panic or early return before the caller's explicit cleanup must
        // never strand an active staged manifest. Normal error handling marks
        // CleanupAttempted first so an injected removal failure remains
        // inspectable by its test instead of being silently retried here.
        if self.state.get() == GraphPublicationState::Staged {
            let errors = self.cleanup_failed_stage(false);
            if !errors.is_empty() {
                log::error!(
                    "Failed to clean abandoned graph publication stage: {}",
                    errors.join("; ")
                );
            }
        }
    }
}

#[derive(Default)]
pub(super) struct DuplicateGraphCopyCounts {
    pub(super) documents: usize,
    pub(super) artifacts: usize,
    pub(super) images: usize,
}

pub(super) fn copy_duplicate_graph_files(
    source_graph_dir: &Path,
    staged_graph_dir: &Path,
    published_graph_dir: &Path,
    source_graph_id: &str,
    new_graph_id: &str,
) -> Result<DuplicateGraphCopyCounts, String> {
    replace_duplicate_documents_from_source(source_graph_dir, staged_graph_dir)?;
    for dirname in ["ydocs", "artifacts", "images"] {
        replace_directory_from_source(
            &source_graph_dir.join(dirname),
            &staged_graph_dir.join(dirname),
        )?;
    }
    ensure_graph_index_dirs(staged_graph_dir)?;
    rewrite_duplicate_graph_manifests(
        staged_graph_dir,
        published_graph_dir,
        source_graph_id,
        new_graph_id,
    )
}

fn replace_duplicate_documents_from_source(
    source_graph_dir: &Path,
    staged_graph_dir: &Path,
) -> Result<(), String> {
    let source_root = documents_dir(source_graph_dir);
    let target_root = documents_dir(staged_graph_dir);
    if target_root.exists() {
        remove_dir_all(&target_root)
            .map_err(|error| format!("remove duplicate target: {error}"))?;
    }
    create_dir_all(&target_root)?;
    if !source_root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(&source_root).map_err(|error| {
        format!(
            "read duplicate source documents {}: {error}",
            display_path(&source_root)
        )
    })? {
        let entry = entry.map_err(|error| format!("read duplicate document entry: {error}"))?;
        let source_path = entry.path();
        let target_path = target_root.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| format!("read duplicate document entry type: {error}"))?;
        if file_type.is_file() {
            copy_file_atomic(&source_path, &target_path).map_err(|error| {
                format!(
                    "copy duplicate file {} -> {}: {error}",
                    display_path(&source_path),
                    display_path(&target_path)
                )
            })?;
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }

        let document_id = entry.file_name().into_string().map_err(|_| {
            format!(
                "duplicate document directory name is not valid UTF-8: {}",
                display_path(&source_path)
            )
        })?;
        create_dir_all(&target_path)?;
        for child in fs::read_dir(&source_path).map_err(|error| {
            format!(
                "read duplicate source document {}: {error}",
                display_path(&source_path)
            )
        })? {
            let child = child.map_err(|error| format!("read duplicate document file: {error}"))?;
            if child.file_name() == std::ffi::OsStr::new("history") {
                continue;
            }
            copy_duplicate_entry(&child, &target_path.join(child.file_name()))?;
        }

        // Manual snapshot RMWs intentionally do not take the graph lifecycle
        // lease. Copy history only under their per-document crash-released
        // lock, including the absent-directory check, so a concurrent snapshot
        // linearizes wholly before or wholly after this source snapshot.
        crate::document_history_service::with_document_history_read_lock(
            source_graph_dir,
            &document_id,
            || {
                let source_history = source_path.join("history");
                if source_history.is_dir() {
                    copy_directory_contents(&source_history, &target_path.join("history"))?;
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}

fn copy_duplicate_entry(entry: &fs::DirEntry, target_path: &Path) -> Result<(), String> {
    let file_type = entry
        .file_type()
        .map_err(|error| format!("read duplicate entry type: {error}"))?;
    let source_path = entry.path();
    if file_type.is_dir() {
        copy_directory_contents(&source_path, target_path)
    } else if file_type.is_file() {
        copy_file_atomic(&source_path, target_path)
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "copy duplicate file {} -> {}: {error}",
                    display_path(&source_path),
                    display_path(target_path)
                )
            })
    } else {
        Ok(())
    }
}

fn replace_directory_from_source(source: &Path, target: &Path) -> Result<(), String> {
    if target.exists() {
        remove_dir_all(target).map_err(|error| format!("remove duplicate target: {error}"))?;
    }
    if source.is_dir() {
        copy_directory_contents(source, target)
    } else {
        create_dir_all(target).map_err(Into::into)
    }
}

fn copy_directory_contents(source: &Path, target: &Path) -> Result<(), String> {
    create_dir_all(target)?;
    for entry in fs::read_dir(source)
        .map_err(|error| format!("read duplicate source {}: {error}", display_path(source)))?
    {
        let entry = entry.map_err(|error| format!("read duplicate entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("read duplicate entry type: {error}"))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_directory_contents(&source_path, &target_path)?;
        } else if file_type.is_file() {
            copy_file_atomic(&source_path, &target_path).map_err(|error| {
                format!(
                    "copy duplicate file {} -> {}: {error}",
                    display_path(&source_path),
                    display_path(&target_path)
                )
            })?;
        }
    }
    Ok(())
}

fn rewrite_duplicate_graph_manifests(
    staged_graph_dir: &Path,
    published_graph_dir: &Path,
    source_graph_id: &str,
    new_graph_id: &str,
) -> Result<DuplicateGraphCopyCounts, String> {
    let mut counts = DuplicateGraphCopyCounts::default();
    rewrite_duplicate_workspace_ydoc(staged_graph_dir, source_graph_id, new_graph_id)?;
    rewrite_duplicate_workspace_snapshot(staged_graph_dir, source_graph_id, new_graph_id)?;
    let document_root = documents_dir(staged_graph_dir);
    create_dir_all(&document_root)?;
    for entry in fs::read_dir(&document_root)
        .map_err(|error| format!("read duplicate documents directory: {error}"))?
    {
        let entry = entry.map_err(|error| format!("read duplicate document entry: {error}"))?;
        if !entry.path().is_dir() {
            continue;
        }
        rewrite_duplicate_document_history(&entry.path(), source_graph_id, new_graph_id)?;
        let document_path = entry.path().join("document.json");
        if !document_path.is_file() {
            continue;
        }
        let mut document = read_json::<serde_json::Value>(&document_path)?;
        rewrite_duplicate_json_references(&mut document, source_graph_id, new_graph_id);
        let object = document.as_object_mut().ok_or_else(|| {
            format!(
                "duplicate document record is not an object: {}",
                display_path(&document_path)
            )
        })?;
        let document_id = object
            .get("documentId")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "duplicate document record is missing documentId: {}",
                    display_path(&document_path)
                )
            })?
            .to_string();
        object.insert(
            "graphId".to_string(),
            serde_json::Value::String(new_graph_id.to_string()),
        );
        object.insert(
            "localPath".to_string(),
            serde_json::Value::String(display_path(
                &documents_dir(published_graph_dir).join(&document_id),
            )),
        );
        object.insert(
            "ydocStatePath".to_string(),
            serde_json::Value::String(display_path(&document_ydoc_state_path(
                published_graph_dir,
                &document_id,
            ))),
        );
        rewrite_duplicate_document_ydoc(
            staged_graph_dir,
            &document_id,
            source_graph_id,
            new_graph_id,
            &mut document,
        )?;
        write_json(&document_path, &document)?;
        rewrite_original_manifest_for_publication(
            &entry.path().join("original"),
            &documents_dir(published_graph_dir)
                .join(&document_id)
                .join("original"),
        )?;
        counts.documents += 1;
    }

    counts.artifacts = rewrite_original_manifests_for_publication(
        &artifacts_dir(staged_graph_dir),
        &artifacts_dir(published_graph_dir),
    )?;
    counts.images = rewrite_original_manifests_for_publication(
        &images_dir(staged_graph_dir),
        &images_dir(published_graph_dir),
    )?;
    Ok(counts)
}

fn rewrite_duplicate_workspace_ydoc(
    staged_graph_dir: &Path,
    source_graph_id: &str,
    new_graph_id: &str,
) -> Result<(), String> {
    use yrs::updates::decoder::Decode;
    use yrs::{Any, Map, Out, ReadTxn, StateVector, Transact, Update};

    let state_path = workspace_ydoc_state_path(staged_graph_dir);
    if !state_path.is_file() {
        return Ok(());
    }
    let bytes = read_bytes(&state_path)?;
    if bytes.is_empty() {
        return Ok(());
    }
    let update = Update::decode_v1(&bytes).map_err(|error| {
        format!(
            "decode duplicate workspace Y.Doc {}: {error}",
            display_path(&state_path)
        )
    })?;
    let doc = yrs::Doc::new();
    doc.transact_mut().apply_update(update).map_err(|error| {
        format!(
            "apply duplicate workspace Y.Doc {}: {error}",
            display_path(&state_path)
        )
    })?;
    let (wire_maps, storage_maps) = {
        let txn = doc.transact();
        let wire_maps = txn
            .get_map("wires")
            .map(|wires| {
                wires
                    .iter(&txn)
                    .filter_map(|(_, value)| match value {
                        Out::YMap(wire) => Some(wire),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut storage_maps = Vec::new();
        for (root_name, key) in [("documents", "sf_storageKey"), ("artifacts", "storageKey")] {
            let Some(root) = txn.get_map(root_name) else {
                continue;
            };
            storage_maps.extend(root.iter(&txn).filter_map(|(_, value)| match value {
                Out::YMap(child) => Some((child, key)),
                _ => None,
            }));
        }
        (wire_maps, storage_maps)
    };
    let mut changed = false;
    {
        let mut txn = doc.transact_mut();
        for wire in wire_maps {
            for key in ["targetGraphId", "sceneGraphId"] {
                let is_self_reference = matches!(
                    wire.get(&txn, key),
                    Some(Out::Any(Any::String(value))) if value.as_ref() == source_graph_id
                );
                if is_self_reference {
                    wire.insert(&mut txn, key, new_graph_id);
                    changed = true;
                }
            }
        }
        for (child, key) in storage_maps {
            let Some(Out::Any(Any::String(value))) = child.get(&txn, key) else {
                continue;
            };
            let next = rewrite_duplicate_storage_path(&value, source_graph_id, new_graph_id);
            if next != value.as_ref() {
                child.insert(&mut txn, key, next);
                changed = true;
            }
        }
    }
    if changed {
        let update = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        write_bytes(&state_path, &update).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn rewrite_duplicate_workspace_snapshot(
    staged_graph_dir: &Path,
    source_graph_id: &str,
    new_graph_id: &str,
) -> Result<(), String> {
    let snapshot_path = workspace_snapshot_path(staged_graph_dir);
    if !snapshot_path.is_file() {
        return Ok(());
    }
    let mut snapshot = read_json::<serde_json::Value>(&snapshot_path)?;
    let object = snapshot.as_object_mut().ok_or_else(|| {
        format!(
            "duplicate workspace snapshot is not an object: {}",
            display_path(&snapshot_path)
        )
    })?;
    object.insert(
        "graphId".to_string(),
        serde_json::Value::String(new_graph_id.to_string()),
    );
    rewrite_duplicate_json_references(&mut snapshot, source_graph_id, new_graph_id);
    write_json(&snapshot_path, &snapshot).map_err(|error| error.to_string())
}

fn rewrite_duplicate_json_references(
    value: &mut serde_json::Value,
    source_graph_id: &str,
    new_graph_id: &str,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                rewrite_duplicate_json_references(value, source_graph_id, new_graph_id);
            }
        }
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "graphId"
                        | "graph_id"
                        | "sourceGraphId"
                        | "source_graph_id"
                        | "targetGraphId"
                        | "target_graph_id"
                        | "sceneGraphId"
                        | "scene_graph_id"
                ) && value.as_str() == Some(source_graph_id)
                {
                    *value = serde_json::Value::String(new_graph_id.to_string());
                } else if matches!(key.as_str(), "storageKey" | "storage_key" | "sf_storageKey") {
                    if let Some(path) = value.as_str() {
                        let rewritten =
                            rewrite_duplicate_storage_path(path, source_graph_id, new_graph_id);
                        if rewritten != path {
                            *value = serde_json::Value::String(rewritten);
                        }
                    }
                } else {
                    rewrite_duplicate_json_references(value, source_graph_id, new_graph_id);
                }
            }
        }
        _ => {}
    }
}

fn rewrite_duplicate_storage_path(
    value: &str,
    source_graph_id: &str,
    new_graph_id: &str,
) -> String {
    value.replace(
        &format!("graphs/{source_graph_id}/"),
        &format!("graphs/{new_graph_id}/"),
    )
}

fn rewrite_duplicate_document_ydoc(
    staged_graph_dir: &Path,
    document_id: &str,
    source_graph_id: &str,
    new_graph_id: &str,
    document: &mut serde_json::Value,
) -> Result<(), String> {
    use yrs::types::xml::{XmlFragment, XmlOut};
    use yrs::updates::decoder::Decode;
    use yrs::{Any, Out, ReadTxn, StateVector, Transact, Update, Xml, XmlElementRef};

    fn collect_element_tree<T: ReadTxn>(
        txn: &T,
        element: XmlElementRef,
        elements: &mut Vec<XmlElementRef>,
    ) {
        for child in element.children(txn) {
            if let XmlOut::Element(child) = child {
                collect_element_tree(txn, child, elements);
            }
        }
        elements.push(element);
    }

    let state_path = document_ydoc_state_path(staged_graph_dir, document_id);
    if !state_path.is_file() {
        return Ok(());
    }
    let original_bytes = read_bytes(&state_path)?;
    if original_bytes.is_empty() {
        return Ok(());
    }
    let update = Update::decode_v1(&original_bytes).map_err(|error| {
        format!(
            "decode duplicate document Y.Doc {}: {error}",
            display_path(&state_path)
        )
    })?;
    if update.is_empty() {
        // No real Y.Doc-authored content: this is the canonical
        // encoded-empty-doc sentinel `document_sidecar_store::empty_ydoc_update_v1`
        // writes for a document whose content authority is document.json, not
        // the Y.Doc (e.g. one only ever touched through the file-only
        // `save_document` path — its sidecar exists and is non-empty BYTES,
        // but carries zero blocks/deletes). A raw `bytes.is_empty()` check
        // (this function's pre-existing early return above) caught the
        // legacy zero-byte shape; it does not catch this one, and rewriting
        // graph-id self-references + re-deriving `body` from this decode
        // would silently clobber the file-authored content with an empty
        // projection.
        return Ok(());
    }
    let doc = yrs::Doc::new();
    doc.transact_mut().apply_update(update).map_err(|error| {
        format!(
            "apply duplicate document Y.Doc {}: {error}",
            display_path(&state_path)
        )
    })?;
    let elements = {
        let fragment = doc.get_or_insert_xml_fragment("content");
        let txn = doc.transact();
        let mut elements = Vec::new();
        for child in fragment.children(&txn) {
            if let XmlOut::Element(element) = child {
                collect_element_tree(&txn, element, &mut elements);
            }
        }
        elements
    };
    let mut changed = false;
    {
        let mut txn = doc.transact_mut();
        for element in elements {
            for key in ["targetGraphId", "target_graph_id"] {
                let is_self_reference = matches!(
                    element.get_attribute(&txn, key),
                    Some(Out::Any(Any::String(value))) if value.as_ref() == source_graph_id
                );
                if is_self_reference {
                    element.insert_attribute(&mut txn, key, new_graph_id);
                    changed = true;
                }
            }
        }
    }
    let encoded = if changed {
        let encoded = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        write_bytes(&state_path, &encoded)?;
        encoded
    } else {
        original_bytes
    };
    let projection = crate::crdt_engine::projection::materialize_ydoc(&doc, document_id);
    let object = document.as_object_mut().ok_or_else(|| {
        format!(
            "duplicate document record is not an object: {}",
            display_path(&state_path)
        )
    })?;
    object.insert(
        "body".to_string(),
        serde_json::Value::String(projection.body),
    );
    object.insert("tiptapJson".to_string(), projection.tiptap_json);
    object.insert("tree".to_string(), projection.tree_json);
    object.insert("blocks".to_string(), projection.blocks_json);
    object.insert(
        "tiptapXml".to_string(),
        serde_json::Value::String(crate::crdt_engine::projection::ydoc_to_tiptap_xml(&doc)),
    );
    object.insert(
        "ydocUpdateBase64".to_string(),
        serde_json::Value::String(BASE64_STANDARD.encode(encoded)),
    );
    Ok(())
}

fn rewrite_duplicate_document_history(
    staged_document_dir: &Path,
    source_graph_id: &str,
    new_graph_id: &str,
) -> Result<(), String> {
    let history_dir = staged_document_dir.join("history");
    if !history_dir.is_dir() {
        return Ok(());
    }

    let store_path = history_dir.join(HISTORY_STORE_FILE);
    if store_path.is_file() {
        let mut store = read_json::<serde_json::Value>(&store_path)?;
        rewrite_duplicate_json_references(&mut store, source_graph_id, new_graph_id);
        force_graph_identity(&mut store, new_graph_id, &store_path)?;
        if let Some(snapshots) = store
            .get_mut("snapshots")
            .and_then(|value| value.as_array_mut())
        {
            for snapshot in snapshots {
                force_graph_identity(snapshot, new_graph_id, &store_path)?;
            }
        }
        write_json(&store_path, &store)?;
    }

    let snapshots_dir = history_dir.join("snapshots");
    if snapshots_dir.is_dir() {
        for entry in fs::read_dir(&snapshots_dir).map_err(|error| {
            format!(
                "read duplicate history snapshots {}: {error}",
                display_path(&snapshots_dir)
            )
        })? {
            let entry = entry.map_err(|error| format!("read duplicate snapshot entry: {error}"))?;
            let path = entry.path();
            if !entry
                .file_type()
                .map_err(|error| format!("read {} type: {error}", display_path(&path)))?
                .is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let mut payload = read_json::<serde_json::Value>(&path)?;
            rewrite_duplicate_json_references(&mut payload, source_graph_id, new_graph_id);
            force_graph_identity(&mut payload, new_graph_id, &path)?;
            write_json(&path, &payload)?;
        }
    }

    let tail_path = history_dir.join(DOCUMENT_TAIL_COMMIT_FILE);
    if tail_path.is_file() {
        let mut tail = read_json::<serde_json::Value>(&tail_path)?;
        rewrite_duplicate_json_references(&mut tail, source_graph_id, new_graph_id);
        force_graph_identity(&mut tail, new_graph_id, &tail_path)?;
        write_json(&tail_path, &tail)?;
    }
    Ok(())
}

fn force_graph_identity(
    value: &mut serde_json::Value,
    graph_id: &str,
    path: &Path,
) -> Result<(), String> {
    let object = value.as_object_mut().ok_or_else(|| {
        format!(
            "duplicate identity-bearing JSON is not an object: {}",
            display_path(path)
        )
    })?;
    object.insert(
        "graphId".to_string(),
        serde_json::Value::String(graph_id.to_string()),
    );
    if object.contains_key("graph_id") {
        object.insert(
            "graph_id".to_string(),
            serde_json::Value::String(graph_id.to_string()),
        );
    }
    Ok(())
}

fn rewrite_original_manifests_for_publication(
    staged_root: &Path,
    published_root: &Path,
) -> Result<usize, String> {
    if !staged_root.is_dir() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in fs::read_dir(staged_root)
        .map_err(|error| format!("read {}: {error}", display_path(staged_root)))?
    {
        let entry = entry.map_err(|error| format!("read original manifest entry: {error}"))?;
        if !entry.path().is_dir() {
            continue;
        }
        let staged_original_dir = entry.path().join("original");
        if staged_original_dir.join("manifest.json").is_file() {
            rewrite_original_manifest_for_publication(
                &staged_original_dir,
                &published_root.join(entry.file_name()).join("original"),
            )?;
            count += 1;
        }
    }
    Ok(count)
}

fn rewrite_original_manifest_for_publication(
    staged_original_dir: &Path,
    published_original_dir: &Path,
) -> Result<(), String> {
    let manifest_path = staged_original_dir.join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(());
    }
    let mut manifest = read_original_manifest(staged_original_dir)?;
    manifest.local_path = display_path(&original_manifest_file_path(
        published_original_dir,
        &manifest.filename,
    )?);
    write_json(&manifest_path, &manifest).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_config::{
        ValidationPolicy, GRAPH_STATUS_ACTIVE, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID,
    };
    use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("garden-{label}-{}", Uuid::new_v4()))
    }

    fn graph_record(graph_id: &str, graph_dir: &Path) -> GraphRecord {
        GraphRecord {
            graph_id: graph_id.to_string(),
            title: "Publication target".to_string(),
            description: None,
            status: GRAPH_STATUS_ACTIVE.to_string(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: display_path(graph_dir),
            created_at: "1".to_string(),
            incarnation_id: Some(Uuid::new_v4().to_string()),
            updated_at: "1".to_string(),
            capabilities: Vec::new(),
            created_by_operation_id: None,
            validation_policy: ValidationPolicy::default(),
            content_revision: None,
        }
    }

    fn insert_quad(store: &oxigraph::store::Store, suffix: &str) {
        let subject = format!("http://example.com/{suffix}");
        store
            .insert(QuadRef::new(
                NamedNodeRef::new(&subject).expect("subject IRI"),
                NamedNodeRef::new("http://example.com/p").expect("predicate IRI"),
                NamedNodeRef::new("http://example.com/o").expect("object IRI"),
                GraphNameRef::DefaultGraph,
            ))
            .expect("insert quad");
    }

    #[test]
    fn adopted_hot_layout_never_moves_out_of_its_canonical_path() {
        let profile = temp_dir("publication-adopt-profile");
        let target = profile.join("graphs/adopted");
        let hot_path = target.join("ydocs/workspace/update-v1.bin");
        create_dir_all(hot_path.parent().expect("hot parent")).expect("create hot layout");
        fs::write(&hot_path, b"only-hot-authority").expect("write hot sidecar");
        let interrupted_manifest = target.join(".graph.json.tmp-123-456");
        fs::write(&interrupted_manifest, b"partial-manifest")
            .expect("write interrupted manifest temp");

        {
            let publication = GraphPublicationReservation::acquire(
                &profile,
                target.clone(),
                "adopted",
                ExistingTargetPolicy::AdoptUnpublishedLayout,
            )
            .expect("adopt hot layout");
            assert_eq!(publication.stage_dir(), target);
            assert_eq!(
                fs::read(&hot_path).expect("hot sidecar remains"),
                b"only-hot-authority"
            );
            assert!(
                !interrupted_manifest.exists(),
                "adoption retained its own interrupted manifest temp"
            );
            assert!(!target.join("graph.json").exists());
            // Model an early return/panic cleanup. Drop must preserve the
            // canonical sidecar layout without needing startup stage recovery.
        }

        assert!(target.is_dir());
        assert_eq!(
            fs::read(&hot_path).expect("hot sidecar after drop"),
            b"only-hot-authority"
        );
        assert!(!target.join("graph.json").exists());
        let _ = remove_dir_all(&profile);
    }

    #[test]
    fn post_rename_error_reconciles_to_committed_publication() {
        let profile = temp_dir("publication-ambiguous-rename");
        let target = profile.join("graphs/committed");
        create_dir_all(target.parent().expect("graphs parent")).expect("create graphs dir");
        let publication = GraphPublicationReservation::acquire(
            &profile,
            target.clone(),
            "committed",
            ExistingTargetPolicy::Reject,
        )
        .expect("reserve publication");
        let graph = graph_record("committed", &target);
        write_json(&publication.stage_dir().join("graph.json"), &graph)
            .expect("write staged manifest");
        let expected = serde_json::to_value(&graph).expect("expected manifest");

        // Model NFS committing the rename but returning an error to the client.
        fs::rename(publication.stage_dir(), &target).expect("commit modeled rename");
        publication
            .reconcile_ambiguous_publication_error(
                &expected,
                "injected post-rename transport error".to_string(),
            )
            .expect("target manifest proves publication committed");
        assert_eq!(publication.state.get(), GraphPublicationState::Published);
        drop(publication);

        let stored: GraphRecord = read_json(&target.join("graph.json"))
            .expect("published graph remains after reservation drop");
        assert_eq!(stored.incarnation_id, graph.incarnation_id);
        let _ = remove_dir_all(&profile);
    }

    #[test]
    fn publication_boundary_evicts_a_removed_same_path_rdf_incarnation() {
        let profile = temp_dir("publication-rdf-profile");
        let target = profile.join("graphs/recreated");
        create_dir_all(&target).expect("create old target");
        let old_store = crate::rdf_store_service::open_graph_store(&target).expect("old store");
        insert_quad(&old_store, "old");
        assert_eq!(old_store.len().expect("old len"), 1);
        let old_store_weak = std::sync::Arc::downgrade(&old_store);
        let detached_old = profile.join("detached-old-incarnation");
        fs::rename(&target, &detached_old).expect("detach old incarnation");
        drop(old_store);
        assert!(
            old_store_weak.upgrade().is_some(),
            "path cache should still own the detached old incarnation before publish"
        );

        let publication = GraphPublicationReservation::acquire(
            &profile,
            target.clone(),
            "recreated",
            ExistingTargetPolicy::Reject,
        )
        .expect("reserve replacement");
        let replacement_store = crate::rdf_store_service::open_graph_store(publication.stage_dir())
            .expect("replacement stage store");
        insert_quad(&replacement_store, "new-one");
        insert_quad(&replacement_store, "new-two");
        drop(replacement_store);
        evict_graph_store(publication.stage_dir()).expect("evict staging handle");
        publication
            .publish_graph_record(&graph_record("recreated", &target))
            .expect("publish replacement");
        assert!(
            old_store_weak.upgrade().is_none(),
            "publication did not evict the final path's stale store"
        );

        let reopened = crate::rdf_store_service::open_graph_store(&target)
            .expect("open replacement at final path");
        assert_eq!(reopened.len().expect("replacement len"), 2);
        evict_graph_store(&target).expect("evict replacement");
        drop(reopened);
        let _ = remove_dir_all(&detached_old);
        let _ = remove_dir_all(&profile);
    }

    #[test]
    fn duplicate_rewrites_workspace_history_payload_and_tail_identities() {
        use yrs::updates::decoder::Decode;
        use yrs::{Any, Map, MapPrelim, Out, ReadTxn, StateVector, Transact, Update, WriteTxn};

        let root = temp_dir("duplicate-identities");
        let source = root.join("source");
        let stage = root.join("stage");
        let published = root.join("published");
        let workspace_path = workspace_snapshot_path(&source);
        create_dir_all(workspace_path.parent().expect("workspace parent"))
            .expect("create workspace dir");
        write_json(
            &workspace_path,
            &serde_json::json!({
                "graphId": "source-graph",
                "wires": [{
                    "targetGraphId": "source-graph",
                    "sceneGraphId": "external-graph"
                }],
                "documents": [{
                    "id": "doc-a",
                    "sf_storageKey": "users/default/graphs/source-graph/documents/doc-a/source.md"
                }],
                "artifacts": [{
                    "id": "artifact-a",
                    "storageKey": "users/default/graphs/source-graph/artifacts/artifact-a/source.pdf"
                }],
                "externalStorage": {
                    "storageKey": "users/default/graphs/external-graph/artifacts/external/source.pdf"
                },
                "unknownFutureField": { "kept": true }
            }),
        )
        .expect("write workspace snapshot");
        let workspace_doc = yrs::Doc::new();
        {
            let mut txn = workspace_doc.transact_mut();
            let wires = txn.get_or_insert_map("wires");
            let wire = wires.insert(&mut txn, "wire-a", MapPrelim::default());
            wire.insert(&mut txn, "targetGraphId", "source-graph");
            wire.insert(&mut txn, "sceneGraphId", "external-graph");
            let documents = txn.get_or_insert_map("documents");
            let document = documents.insert(&mut txn, "doc-a", MapPrelim::default());
            document.insert(
                &mut txn,
                "sf_storageKey",
                "users/default/graphs/source-graph/documents/doc-a/source.md",
            );
            let artifacts = txn.get_or_insert_map("artifacts");
            let artifact = artifacts.insert(&mut txn, "artifact-a", MapPrelim::default());
            artifact.insert(
                &mut txn,
                "storageKey",
                "users/default/graphs/source-graph/artifacts/artifact-a/source.pdf",
            );
        }
        let workspace_update = workspace_doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        write_bytes(&workspace_ydoc_state_path(&source), &workspace_update)
            .expect("write workspace Y.Doc");

        let source_document_doc =
            crate::crdt_engine::builder::ydoc_from_tiptap_json(&serde_json::json!({
                "type": "doc",
                "content": [{
                    "type": "paragraph",
                    "attrs": { "data-block-id": "block-a" },
                    "content": [
                        {
                            "type": "wikilink",
                            "attrs": {
                                "targetDocId": "doc-a",
                                "targetGraphId": "source-graph",
                                "label": "Self"
                            }
                        },
                        {
                            "type": "wikilink",
                            "attrs": {
                                "targetDocId": "external-doc",
                                "targetGraphId": "external-graph",
                                "label": "External"
                            }
                        }
                    ]
                }]
            }));
        let source_document_update = source_document_doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        write_bytes(
            &document_ydoc_state_path(&source, "doc-a"),
            &source_document_update,
        )
        .expect("write document Y.Doc");
        let source_projection =
            crate::crdt_engine::projection::materialize_ydoc(&source_document_doc, "doc-a");
        let source_tiptap_xml =
            crate::crdt_engine::projection::ydoc_to_tiptap_xml(&source_document_doc);
        let source_document_dir = source.join("documents/doc-a");
        create_dir_all(&source_document_dir).expect("create source document dir");
        write_json(
            &source_document_dir.join("document.json"),
            &serde_json::json!({
                "documentId": "doc-a",
                "graphId": "source-graph",
                "title": "Identity document",
                "revision": 1,
                "body": source_projection.body,
                "origin": "local",
                "providerId": "local",
                "localPath": display_path(&source_document_dir),
                "rdfSubject": "urn:mnemosyne:local:document:doc-a",
                "createdAt": "1",
                "updatedAt": "1",
                "capabilities": [],
                "schemaVersion": 1,
                "tiptapXml": source_tiptap_xml,
                "tiptapJson": source_projection.tiptap_json,
                "ydocUpdateBase64": BASE64_STANDARD.encode(&source_document_update),
                "ydocStatePath": display_path(&document_ydoc_state_path(&source, "doc-a")),
                "tree": source_projection.tree_json,
                "blocks": source_projection.blocks_json,
                "rdfTripleCount": 0,
                "sourceFile": {
                    "sf_storageKey": "users/default/graphs/source-graph/documents/doc-a/source.md"
                },
                "externalStorage": {
                    "storageKey": "users/default/graphs/external-graph/documents/external/source.md"
                },
                "unknownFutureField": {
                    "opaqueGraphLabel": "source-graph",
                    "kept": true
                }
            }),
        )
        .expect("write source document record");

        let history_dir = source.join("documents/doc-a/history");
        let snapshots_dir = history_dir.join("snapshots");
        create_dir_all(&snapshots_dir).expect("create history dirs");
        write_json(
            &history_dir.join(HISTORY_STORE_FILE),
            &serde_json::json!({
                "schemaVersion": 3,
                "graphId": "source-graph",
                "documentId": "doc-a",
                "snapshots": [{
                    "snapshotId": "snap-a",
                    "graphId": "source-graph",
                    "documentId": "doc-a"
                }],
                "unknownFutureField": "preserved"
            }),
        )
        .expect("write history store");
        write_json(
            &snapshots_dir.join("snap-a.json"),
            &serde_json::json!({
                "snapshotId": "snap-a",
                "graphId": "source-graph",
                "documentId": "doc-a",
                "blocks": [],
                "nested": {
                    "targetGraphId": "source-graph",
                    "externalGraphId": "external-graph",
                    "storageKey": "users/default/graphs/source-graph/documents/doc-a/history"
                },
                "unknownFutureField": 7
            }),
        )
        .expect("write snapshot payload");
        write_json(
            &history_dir.join(DOCUMENT_TAIL_COMMIT_FILE),
            &serde_json::json!({
                "schemaVersion": 1,
                "graphId": "source-graph",
                "documentId": "doc-a",
                "snapshotId": "snap-a",
                "graphContentRevision": "10"
            }),
        )
        .expect("write tail commit");

        copy_duplicate_graph_files(&source, &stage, &published, "source-graph", "target-graph")
            .expect("copy duplicate identities");

        let workspace: serde_json::Value =
            read_json(&workspace_snapshot_path(&stage)).expect("read target workspace");
        assert_eq!(workspace["graphId"], "target-graph");
        assert_eq!(workspace["wires"][0]["targetGraphId"], "target-graph");
        assert_eq!(workspace["wires"][0]["sceneGraphId"], "external-graph");
        assert_eq!(
            workspace["documents"][0]["sf_storageKey"],
            "users/default/graphs/target-graph/documents/doc-a/source.md"
        );
        assert_eq!(
            workspace["artifacts"][0]["storageKey"],
            "users/default/graphs/target-graph/artifacts/artifact-a/source.pdf"
        );
        assert_eq!(
            workspace["externalStorage"]["storageKey"],
            "users/default/graphs/external-graph/artifacts/external/source.pdf"
        );
        assert_eq!(workspace["unknownFutureField"]["kept"], true);
        let copied_workspace = yrs::Doc::new();
        let copied_update = Update::decode_v1(
            &read_bytes(&workspace_ydoc_state_path(&stage)).expect("read copied workspace Y.Doc"),
        )
        .expect("decode copied workspace Y.Doc");
        copied_workspace
            .transact_mut()
            .apply_update(copied_update)
            .expect("apply copied workspace Y.Doc");
        let rematerialized =
            crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(
                "target-graph",
                &copied_workspace,
            )
            .expect("materialize copied workspace Y.Doc");
        assert_eq!(rematerialized["wires"][0]["targetGraphId"], "target-graph");
        assert_eq!(rematerialized["wires"][0]["sceneGraphId"], "external-graph");
        {
            let txn = copied_workspace.transact();
            let documents = txn.get_map("documents").expect("copied documents map");
            let Out::YMap(document) = documents.get(&txn, "doc-a").expect("copied document map")
            else {
                panic!("copied document entry is not a map");
            };
            assert!(matches!(
                document.get(&txn, "sf_storageKey"),
                Some(Out::Any(Any::String(value)))
                    if value.as_ref()
                        == "users/default/graphs/target-graph/documents/doc-a/source.md"
            ));
            let artifacts = txn.get_map("artifacts").expect("copied artifacts map");
            let Out::YMap(artifact) = artifacts.get(&txn, "artifact-a").expect("artifact map")
            else {
                panic!("copied artifact entry is not a map");
            };
            assert!(matches!(
                artifact.get(&txn, "storageKey"),
                Some(Out::Any(Any::String(value)))
                    if value.as_ref()
                        == "users/default/graphs/target-graph/artifacts/artifact-a/source.pdf"
            ));
        }

        let target_document: serde_json::Value =
            read_json(&stage.join("documents/doc-a/document.json"))
                .expect("target document record");
        assert_eq!(target_document["graphId"], "target-graph");
        assert_eq!(
            target_document["tiptapJson"]["content"][0]["content"][0]["attrs"]["targetGraphId"],
            "target-graph"
        );
        assert_eq!(
            target_document["tiptapJson"]["content"][0]["content"][1]["attrs"]["targetGraphId"],
            "external-graph"
        );
        assert_eq!(
            target_document["blocks"][0]["marks"][0]["targetGraphId"],
            "target-graph"
        );
        assert_eq!(
            target_document["blocks"][0]["marks"][1]["targetGraphId"],
            "external-graph"
        );
        assert_eq!(
            target_document["sourceFile"]["sf_storageKey"],
            "users/default/graphs/target-graph/documents/doc-a/source.md"
        );
        assert_eq!(
            target_document["externalStorage"]["storageKey"],
            "users/default/graphs/external-graph/documents/external/source.md"
        );
        assert_eq!(
            target_document["unknownFutureField"]["opaqueGraphLabel"],
            "source-graph"
        );
        assert_eq!(target_document["unknownFutureField"]["kept"], true);
        let copied_document = yrs::Doc::new();
        let copied_document_update = Update::decode_v1(
            &read_bytes(&document_ydoc_state_path(&stage, "doc-a"))
                .expect("read copied document Y.Doc"),
        )
        .expect("decode copied document Y.Doc");
        copied_document
            .transact_mut()
            .apply_update(copied_document_update)
            .expect("apply copied document Y.Doc");
        let copied_document_json =
            crate::crdt_engine::projection::ydoc_to_tiptap_json(&copied_document);
        assert_eq!(
            copied_document_json["content"][0]["content"][0]["attrs"]["targetGraphId"],
            "target-graph"
        );
        assert_eq!(
            copied_document_json["content"][0]["content"][1]["attrs"]["targetGraphId"],
            "external-graph"
        );

        let target_history = stage.join("documents/doc-a/history");
        let history: serde_json::Value =
            read_json(&target_history.join(HISTORY_STORE_FILE)).expect("target history");
        assert_eq!(history["graphId"], "target-graph");
        assert_eq!(history["snapshots"][0]["graphId"], "target-graph");
        assert_eq!(history["unknownFutureField"], "preserved");
        let payload: serde_json::Value = read_json(&target_history.join("snapshots/snap-a.json"))
            .expect("target snapshot payload");
        assert_eq!(payload["graphId"], "target-graph");
        assert_eq!(payload["nested"]["targetGraphId"], "target-graph");
        assert_eq!(payload["nested"]["externalGraphId"], "external-graph");
        assert_eq!(
            payload["nested"]["storageKey"],
            "users/default/graphs/target-graph/documents/doc-a/history"
        );
        assert_eq!(payload["unknownFutureField"], 7);
        let tail: serde_json::Value =
            read_json(&target_history.join(DOCUMENT_TAIL_COMMIT_FILE)).expect("target tail");
        assert_eq!(tail["graphId"], "target-graph");

        let _ = remove_dir_all(&root);
    }

    #[test]
    fn duplicate_preserves_document_tombstone_over_copied_stale_sidecar() {
        crate::app_runtime::async_runtime::block_on(async {
            let root = temp_dir("duplicate-document-tombstone");
            let source = root.join("source");
            let stage = root.join("stage");
            let published = root.join("published");
            let document_id = "deleted-document";
            let source_state = document_ydoc_state_path(&source, document_id);
            let source_registry = crate::crdt_engine::rooms::RoomRegistry::default();
            let room = source_registry
                .get_or_create("doc:source-graph:deleted-document", source_state.clone())
                .await
                .expect("create stale source room");
            room.update_doc(|_doc, txn| {
                use yrs::{Map, WriteTxn};
                txn.get_or_insert_map("metadata")
                    .insert(txn, "deleted-sentinel", true);
                Ok(())
            })
            .await
            .expect("persist stale source sidecar");
            source_registry.evict_room("doc:source-graph:deleted-document");
            let source_tombstone =
                crate::document_tombstone_store::write_document_tombstone_for_operation(
                    &source,
                    document_id,
                    Some("delete-before-duplicate"),
                )
                .expect("write source tombstone");

            copy_duplicate_graph_files(&source, &stage, &published, "source-graph", "target-graph")
                .expect("copy duplicate with tombstone");

            assert!(document_ydoc_state_path(&stage, document_id).is_file());
            assert_eq!(
                crate::document_tombstone_store::read_document_tombstone(&stage, document_id)
                    .expect("read target tombstone")
                    .expect("target tombstone preserved"),
                source_tombstone
            );
            let target_registry = crate::crdt_engine::rooms::RoomRegistry::default();
            let error = match target_registry
                .get_or_create(
                    "doc:target-graph:deleted-document",
                    document_ydoc_state_path(&stage, document_id),
                )
                .await
            {
                Ok(_) => panic!("duplicated stale sidecar must remain tombstoned"),
                Err(error) => error,
            };
            assert!(error.contains("document tombstoned"), "{error}");
            assert!(target_registry
                .peek("doc:target-graph:deleted-document")
                .await
                .is_none());

            let _ = remove_dir_all(&root);
        });
    }

    #[test]
    fn duplicate_history_copy_waits_for_manual_snapshot_lock() {
        let root = temp_dir("duplicate-history-lock");
        let source = root.join("source");
        let stage = root.join("stage");
        let published = root.join("published");
        let document_id = "doc-locked";
        let history_dir = source.join("documents").join(document_id).join("history");
        create_dir_all(&history_dir).expect("create source history");
        write_json(
            &history_dir.join(HISTORY_STORE_FILE),
            &serde_json::json!({
                "schemaVersion": 3,
                "graphId": "source-graph",
                "documentId": document_id,
                "snapshots": [],
                "writerState": "before"
            }),
        )
        .expect("write initial history");

        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder_source = source.clone();
        let holder_history = history_dir.clone();
        let holder = std::thread::spawn(move || {
            crate::document_history_service::with_document_history_read_lock(
                &holder_source,
                document_id,
                || {
                    write_json(
                        &holder_history.join(HISTORY_STORE_FILE),
                        &serde_json::json!({
                            "schemaVersion": 3,
                            "graphId": "source-graph",
                            "documentId": document_id,
                            "snapshots": [],
                            "writerState": "committed-under-lock"
                        }),
                    )
                    .map_err(|error| error.to_string())?;
                    locked_tx.send(()).expect("signal held history lock");
                    release_rx.recv().expect("release held history lock");
                    Ok(())
                },
            )
        });
        locked_rx.recv().expect("history lock acquired");
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        crate::document_history_service::install_document_history_lock_attempt_hook(
            &source,
            document_id,
            attempt_tx,
        )
        .expect("install duplicate history-lock attempt hook");

        let copy_source = source.clone();
        let copy_stage = stage.clone();
        let copy_published = published.clone();
        let (copy_done_tx, copy_done_rx) = std::sync::mpsc::channel();
        let copy = std::thread::spawn(move || {
            let result = copy_duplicate_graph_files(
                &copy_source,
                &copy_stage,
                &copy_published,
                "source-graph",
                "target-graph",
            );
            copy_done_tx.send(result).expect("send copy result");
        });

        attempt_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("duplicate attempted the held history lock");
        let early = copy_done_rx.recv_timeout(std::time::Duration::from_millis(50));
        let copy_was_blocked = matches!(&early, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        release_tx.send(()).expect("release history writer");
        holder
            .join()
            .expect("history holder thread")
            .expect("history holder result");
        let copy_result = match early {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => copy_done_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("copy resumed after history release"),
            Err(error) => panic!("copy result channel disconnected: {error}"),
        };
        copy_result.expect("duplicate copy succeeds");
        copy.join().expect("copy thread");
        crate::document_history_service::clear_document_history_lock_attempt_hook();
        assert!(
            copy_was_blocked,
            "duplicate history copy bypassed the manual snapshot lock"
        );

        let copied: serde_json::Value = read_json(
            &stage
                .join("documents")
                .join(document_id)
                .join("history")
                .join(HISTORY_STORE_FILE),
        )
        .expect("read copied locked history");
        assert_eq!(copied["writerState"], "committed-under-lock");
        assert_eq!(copied["graphId"], "target-graph");
        let _ = remove_dir_all(&root);
    }
}
