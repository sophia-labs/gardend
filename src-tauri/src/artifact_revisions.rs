//! Versioned artifact revisions — binary-artifact analog of document restore
//! points (`time_travel_*`).
//!
//! The artifact's `original/` bytes are the **live state** (every existing
//! reader — sidebar thumbnail, viewer, `get_workspace`, RDF, the download route
//! — reads it). This module keeps a parallel full-snapshot history under
//! `artifacts/{id}/revisions/`:
//!
//! ```text
//! artifacts/{id}/
//!   original/   manifest.json + <bytes>     # live
//!   revisions/
//!     index.json                            # ArtifactRevisionIndex
//!     {rev_id}/ manifest.json + <bytes>     # full snapshot
//! ```
//!
//! Save = snapshot the pre-edit original as a baseline (first time only), write
//! the new bytes to `original/` (live) **and** a new revision. Restore =
//! checkpoint current, then copy a revision back to `original/`. Reuses the
//! `original_file_*` atomic write/read helpers; CRDT/diff/tier machinery from
//! the document side does not apply to binary images.

use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    ids::validate_local_id,
    original_file_service::read_artifact_original_file,
    original_file_storage::{read_original_file_from_dir, save_original_file_to_dir},
    paths::{artifacts_dir, existing_graph_dir},
    storage::{read_json, write_json},
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const REVISION_INDEX_FILE: &str = "index.json";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RevisionTrigger {
    /// An explicit user save from the editor.
    Manual,
    /// An automatic snapshot the system took to make an operation reversible
    /// (the pre-edit baseline, and the pre-restore checkpoint).
    Checkpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactRevisionEntry {
    pub(crate) revision_id: String,
    pub(crate) created_at: u128,
    pub(crate) trigger: RevisionTrigger,
    #[serde(default)]
    pub(crate) label: Option<String>,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) size_bytes: usize,
    pub(crate) content_hash_sha256: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactRevisionIndex {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    entries: Vec<ArtifactRevisionEntry>,
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn revisions_dir(app: &AppHandle, graph_id: &str, artifact_id: &str) -> AppResult<PathBuf> {
    validate_local_id(artifact_id, "artifact_id").map_err(AppError::validation)?;
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    Ok(artifacts_dir(&graph_dir)
        .join(artifact_id)
        .join("revisions"))
}

fn index_path(dir: &std::path::Path) -> PathBuf {
    dir.join(REVISION_INDEX_FILE)
}

fn read_index(dir: &std::path::Path) -> AppResult<ArtifactRevisionIndex> {
    let path = index_path(dir);
    if !path.exists() {
        return Ok(ArtifactRevisionIndex::default());
    }
    read_json(&path).map_err(Into::into)
}

fn write_index(dir: &std::path::Path, mut index: ArtifactRevisionIndex) -> AppResult<()> {
    index.schema_version = SCHEMA_VERSION;
    write_json(&index_path(dir), &index).map_err(Into::into)
}

fn new_revision_id() -> String {
    format!("rev-{}-{}", now_millis(), uuid::Uuid::new_v4().simple())
}

/// Write `bytes` (base64) as a new revision under `revisions/{rev}/` and append
/// an index entry. Returns the entry.
fn snapshot_revision(
    revisions_dir: &std::path::Path,
    filename: &str,
    mime_type: &str,
    data_base64: &str,
    trigger: RevisionTrigger,
    label: Option<String>,
) -> AppResult<ArtifactRevisionEntry> {
    let revision_id = new_revision_id();
    let revision_dir = revisions_dir.join(&revision_id);
    let (manifest, bytes) =
        save_original_file_to_dir(&revision_dir, filename, mime_type, data_base64)
            .map_err(AppError::storage)?;
    let entry = ArtifactRevisionEntry {
        revision_id,
        created_at: now_millis(),
        trigger,
        label,
        filename: manifest.filename,
        mime_type: manifest.mime_type,
        size_bytes: bytes.len(),
        content_hash_sha256: sha256_hex(&bytes),
    };
    let mut index = read_index(revisions_dir)?;
    index.entries.push(entry.clone());
    write_index(revisions_dir, index)?;
    Ok(entry)
}

/// Create a revision from edited bytes: snapshot the pre-edit original as a
/// baseline (first time only), write the new bytes to the live `original/`, and
/// snapshot them as a new revision. Returns the new revision entry.
pub(crate) fn create_artifact_revision(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    filename: &str,
    mime_type: &str,
    data_base64: &str,
    label: Option<String>,
) -> AppResult<ArtifactRevisionEntry> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(AppError::storage)?;
    crate::artifact_text_service::refuse_legacy_writer(
        &existing_graph_dir(app, graph_id).map_err(AppError::storage)?,
        artifact_id,
    )?;
    let dir = revisions_dir(app, graph_id, artifact_id)?;

    // Baseline: on the first edit, preserve the current original so the
    // pre-edit state is restorable.
    let index = read_index(&dir)?;
    if index.entries.is_empty() {
        if let Ok((manifest, bytes)) = read_artifact_original_file(app, graph_id, artifact_id) {
            let baseline_b64 = BASE64_STANDARD.encode(&bytes);
            snapshot_revision(
                &dir,
                &manifest.filename,
                &manifest.mime_type,
                &baseline_b64,
                RevisionTrigger::Checkpoint,
                Some("Original".to_string()),
            )?;
        }
    }

    // Write the edit to the live original (so all existing readers reflect it).
    crate::original_file_service::save_artifact_original_file_with_lease(
        app,
        graph_id,
        artifact_id,
        filename,
        mime_type,
        data_base64,
    )?;

    // And snapshot it as the new revision.
    snapshot_revision(
        &dir,
        filename,
        mime_type,
        data_base64,
        RevisionTrigger::Manual,
        label,
    )
}

/// List revisions newest-first.
pub(crate) fn list_artifact_revisions(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> AppResult<Vec<ArtifactRevisionEntry>> {
    let dir = revisions_dir(app, graph_id, artifact_id)?;
    let mut entries = read_index(&dir)?.entries;
    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(entries)
}

/// Read one revision's manifest + bytes (for preview / download / restore).
pub(crate) fn read_artifact_revision(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    revision_id: &str,
) -> AppResult<(crate::original_file_types::OriginalFileManifest, Vec<u8>)> {
    validate_local_id(revision_id, "revision_id").map_err(AppError::validation)?;
    let dir = revisions_dir(app, graph_id, artifact_id)?.join(revision_id);
    read_original_file_from_dir(&dir).map_err(AppError::storage)
}

/// Restore a revision: checkpoint the current live bytes (so restore is itself
/// reversible), then copy the revision's bytes back to the live `original/`.
/// Returns the checkpoint entry that captured the pre-restore state.
pub(crate) fn restore_artifact_revision(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    revision_id: &str,
) -> AppResult<ArtifactRevisionEntry> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(AppError::storage)?;
    crate::artifact_text_service::refuse_legacy_writer(
        &existing_graph_dir(app, graph_id).map_err(AppError::storage)?,
        artifact_id,
    )?;
    let dir = revisions_dir(app, graph_id, artifact_id)?;
    // Snapshot current live state as a checkpoint first.
    let (current_manifest, current_bytes) =
        read_artifact_original_file(app, graph_id, artifact_id)?;
    let checkpoint = snapshot_revision(
        &dir,
        &current_manifest.filename,
        &current_manifest.mime_type,
        &BASE64_STANDARD.encode(&current_bytes),
        RevisionTrigger::Checkpoint,
        Some("Before restore".to_string()),
    )?;
    // Copy the chosen revision's bytes to the live original.
    let (rev_manifest, rev_bytes) =
        read_artifact_revision(app, graph_id, artifact_id, revision_id)?;
    crate::original_file_service::save_artifact_original_file_with_lease(
        app,
        graph_id,
        artifact_id,
        &rev_manifest.filename,
        &rev_manifest.mime_type,
        &BASE64_STANDARD.encode(&rev_bytes),
    )?;
    Ok(checkpoint)
}

/// Publish an already durable deterministic text snapshot exactly once. This
/// updates the existing history index; it does not change current bytes.
pub(crate) fn publish_text_snapshot(
    root: &std::path::Path,
    id: &str,
    created_at: u128,
    checkpoint: bool,
) -> AppResult<()> {
    let _guard = crate::cell_durability::write_guard();
    let dir = root.join("revisions");
    let (manifest, bytes) =
        read_original_file_from_dir(&dir.join(id)).map_err(AppError::storage)?;
    let entry = ArtifactRevisionEntry {
        revision_id: id.into(),
        created_at,
        trigger: if checkpoint {
            RevisionTrigger::Checkpoint
        } else {
            RevisionTrigger::Manual
        },
        label: None,
        filename: manifest.filename,
        mime_type: manifest.mime_type,
        size_bytes: bytes.len(),
        content_hash_sha256: sha256_hex(&bytes),
    };
    let mut index = read_index(&dir)?;
    if index.schema_version != 0 && index.schema_version != SCHEMA_VERSION {
        return Err(AppError::validation("unsupported revision index version"));
    }
    if let Some(old) = index.entries.iter().find(|old| old.revision_id == id) {
        if old.content_hash_sha256 != entry.content_hash_sha256
            || old.filename != entry.filename
            || old.mime_type != entry.mime_type
        {
            return Err(AppError::conflict("text history entry changed"));
        }
        return Ok(());
    }
    index.entries.push(entry);
    write_index(&dir, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_stable_and_hex() {
        let h = sha256_hex(b"hello");
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(h.len(), 64);
    }

    #[test]
    fn revision_ids_are_unique_and_prefixed() {
        let a = new_revision_id();
        let b = new_revision_id();
        assert!(a.starts_with("rev-"));
        assert_ne!(a, b);
    }

    #[test]
    fn trigger_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&RevisionTrigger::Manual).unwrap(),
            "\"manual\""
        );
        assert_eq!(
            serde_json::to_string(&RevisionTrigger::Checkpoint).unwrap(),
            "\"checkpoint\""
        );
    }

    #[test]
    fn index_round_trips() {
        let dir = std::env::temp_dir().join(format!("artrev-{}", now_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = ArtifactRevisionEntry {
            revision_id: "rev-1-abc".to_string(),
            created_at: 1,
            trigger: RevisionTrigger::Manual,
            label: Some("x".to_string()),
            filename: "a.png".to_string(),
            mime_type: "image/png".to_string(),
            size_bytes: 3,
            content_hash_sha256: "deadbeef".to_string(),
        };
        write_index(
            &dir,
            ArtifactRevisionIndex {
                schema_version: 0,
                entries: vec![entry.clone()],
            },
        )
        .unwrap();
        let read = read_index(&dir).unwrap();
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].revision_id, "rev-1-abc");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
