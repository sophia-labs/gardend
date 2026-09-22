use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    graph_service::touch_graph_updated_at,
    ids::validate_local_id,
    original_file_storage::{
        read_original_file_from_dir, save_original_file_from_path_to_dir, save_original_file_to_dir,
    },
    paths::{
        artifact_original_dir, artifacts_dir, existing_document_dir, existing_graph_dir,
        existing_graph_dir_no_heal, image_original_dir, validate_pending_upload_path,
    },
    storage::{remove_dir_all, remove_file_if_exists},
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::path::Path;

pub(super) use crate::original_file_access_tokens::{
    image_access_token_matches, write_image_access_token,
};
pub(super) use crate::original_file_http::{
    image_file_response, original_file_download_response, query_bool,
};
pub(super) use crate::original_file_storage::{
    rewrite_original_manifest_local_path, rewrite_original_manifests_under,
};
pub(super) use crate::original_file_types::{
    AdoptPendingOriginalFileInput, OriginalFileManifest, OriginalFileManifestRecord,
    OriginalFileRecord, SaveOriginalFileInput,
};

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn save_original_file(
    app: AppHandle,
    input: SaveOriginalFileInput,
) -> Result<OriginalFileRecord, String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            &app,
            &input.graph_id,
        )?;
    save_original_file_service(&app, input).map_err(AppError::message)
}

pub(super) fn save_original_file_service(
    app: &AppHandle,
    input: SaveOriginalFileInput,
) -> AppResult<OriginalFileRecord> {
    let _flush_guard = crate::cell_durability::write_guard();
    let SaveOriginalFileInput {
        graph_id,
        document_id,
        filename,
        mime_type,
        data_base64,
    } = input;
    let graph_dir = existing_graph_dir(app, &graph_id).map_err(AppError::storage)?;
    let document_dir =
        existing_document_dir(&graph_dir, &document_id).map_err(AppError::storage)?;
    let (manifest, bytes) = save_original_file_to_dir(
        &document_dir.join("original"),
        &filename,
        &mime_type,
        &data_base64,
    )
    .map_err(AppError::storage)?;
    touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;

    Ok(OriginalFileRecord {
        graph_id,
        document_id,
        filename: manifest.filename,
        source_filename: manifest.source_filename,
        mime_type: manifest.mime_type,
        size_bytes: manifest.size_bytes,
        local_path: manifest.local_path,
        updated_at: manifest.updated_at,
        data_base64: BASE64_STANDARD.encode(bytes),
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn save_original_file_manifest(
    app: AppHandle,
    input: SaveOriginalFileInput,
) -> Result<OriginalFileManifestRecord, String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            &app,
            &input.graph_id,
        )?;
    save_original_file_manifest_service(&app, input).map_err(AppError::message)
}

pub(super) fn save_original_file_manifest_service(
    app: &AppHandle,
    input: SaveOriginalFileInput,
) -> AppResult<OriginalFileManifestRecord> {
    let _flush_guard = crate::cell_durability::write_guard();
    let SaveOriginalFileInput {
        graph_id,
        document_id,
        filename,
        mime_type,
        data_base64,
    } = input;
    let graph_dir = existing_graph_dir(app, &graph_id).map_err(AppError::storage)?;
    let document_dir =
        existing_document_dir(&graph_dir, &document_id).map_err(AppError::storage)?;
    let (manifest, _) = save_original_file_to_dir(
        &document_dir.join("original"),
        &filename,
        &mime_type,
        &data_base64,
    )
    .map_err(AppError::storage)?;
    touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;

    Ok(OriginalFileManifestRecord {
        graph_id,
        document_id,
        filename: manifest.filename,
        source_filename: manifest.source_filename,
        mime_type: manifest.mime_type,
        size_bytes: manifest.size_bytes,
        local_path: manifest.local_path,
        updated_at: manifest.updated_at,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn adopt_pending_original_file(
    app: AppHandle,
    input: AdoptPendingOriginalFileInput,
) -> Result<OriginalFileManifestRecord, String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            &app,
            &input.graph_id,
        )?;
    adopt_pending_original_file_service(&app, input).map_err(AppError::message)
}

pub(super) fn adopt_pending_original_file_service(
    app: &AppHandle,
    input: AdoptPendingOriginalFileInput,
) -> AppResult<OriginalFileManifestRecord> {
    adopt_pending_original_file_inner(app, input, true)
}

/// Copy a staged original into its durable document location while retaining
/// the pending source. Multi-step CRDT ingest handlers call this variant and
/// remove the pending file only after their Tier-B completion entry is
/// durable, so a crash can replay the exact same outer operation.
pub(crate) fn copy_pending_original_file_service(
    app: &AppHandle,
    input: AdoptPendingOriginalFileInput,
) -> AppResult<OriginalFileManifestRecord> {
    adopt_pending_original_file_inner(app, input, false)
}

fn adopt_pending_original_file_inner(
    app: &AppHandle,
    input: AdoptPendingOriginalFileInput,
    cleanup_pending: bool,
) -> AppResult<OriginalFileManifestRecord> {
    let _flush_guard = crate::cell_durability::write_guard();
    let graph_dir = existing_graph_dir(app, &input.graph_id).map_err(AppError::storage)?;
    let document_dir =
        existing_document_dir(&graph_dir, &input.document_id).map_err(AppError::storage)?;
    let pending_path = validate_pending_upload_path(&graph_dir, &input.pending_path)
        .map_err(AppError::validation)?;
    let manifest = save_original_file_from_path_to_dir(
        &document_dir.join("original"),
        &input.filename,
        &input.mime_type,
        &pending_path,
    )
    .map_err(AppError::storage)?;
    if cleanup_pending {
        remove_file_if_exists(&pending_path)
            .map_err(|error| AppError::storage(format!("remove pending original file: {error}")))?;
    }
    touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;

    Ok(OriginalFileManifestRecord {
        graph_id: input.graph_id,
        document_id: input.document_id,
        filename: manifest.filename,
        source_filename: manifest.source_filename,
        mime_type: manifest.mime_type,
        size_bytes: manifest.size_bytes,
        local_path: manifest.local_path,
        updated_at: manifest.updated_at,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn read_original_file(
    app: AppHandle,
    graph_id: String,
    document_id: String,
) -> Result<OriginalFileRecord, String> {
    read_original_file_command(&app, graph_id, document_id).map_err(AppError::message)
}

fn read_original_file_command(
    app: &AppHandle,
    graph_id: String,
    document_id: String,
) -> AppResult<OriginalFileRecord> {
    let (manifest, bytes) = read_document_original_file(app, &graph_id, &document_id)?;

    Ok(OriginalFileRecord {
        graph_id,
        document_id,
        filename: manifest.filename,
        source_filename: manifest.source_filename,
        mime_type: manifest.mime_type,
        size_bytes: bytes.len(),
        local_path: manifest.local_path,
        updated_at: manifest.updated_at,
        data_base64: BASE64_STANDARD.encode(bytes),
    })
}

pub(super) fn read_document_original_file(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> AppResult<(OriginalFileManifest, Vec<u8>)> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let document_dir = existing_document_dir(&graph_dir, document_id).map_err(AppError::storage)?;
    read_original_file_from_dir(&document_dir.join("original")).map_err(AppError::storage)
}

pub(super) fn save_artifact_original_file(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    filename: &str,
    mime_type: &str,
    data_base64: &str,
) -> AppResult<OriginalFileManifest> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(AppError::storage)?;
    save_artifact_original_file_with_lease(
        app,
        graph_id,
        artifact_id,
        filename,
        mime_type,
        data_base64,
    )
}

pub(crate) fn save_artifact_original_file_with_lease(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    filename: &str,
    mime_type: &str,
    data_base64: &str,
) -> AppResult<OriginalFileManifest> {
    let _flush_guard = crate::cell_durability::write_guard();
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let original_dir = artifact_original_dir(&graph_dir, artifact_id).map_err(AppError::storage)?;
    crate::artifact_text_service::refuse_legacy_writer(&graph_dir, artifact_id)?;
    let (manifest, _) = save_original_file_to_dir(&original_dir, filename, mime_type, data_base64)
        .map_err(AppError::storage)?;
    touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;
    Ok(manifest)
}

pub(super) fn read_artifact_original_file(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> AppResult<(OriginalFileManifest, Vec<u8>)> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let original_dir = artifact_original_dir(&graph_dir, artifact_id).map_err(AppError::storage)?;
    read_original_file_from_dir(&original_dir).map_err(AppError::storage)
}

pub(super) fn delete_artifact_original_files(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> AppResult<()> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::
            acquire_lifecycle_exclusive_blocking_if_managed(app, graph_id)
                .map_err(AppError::storage)?;
    delete_artifact_original_files_with_lease(app, graph_id, artifact_id)
}

fn delete_artifact_original_files_with_lease(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> AppResult<()> {
    let _flush_guard = crate::cell_durability::write_guard();
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    validate_local_id(artifact_id, "artifact_id").map_err(AppError::validation)?;
    let artifact_dir = artifacts_dir(&graph_dir).join(artifact_id);
    if artifact_dir.exists() {
        remove_dir_all(&artifact_dir).map_err(|error| {
            AppError::storage(format!("remove artifact original files: {error}"))
        })?;
        touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;
    }
    Ok(())
}

pub(super) fn adopt_pending_image_file(
    app: &AppHandle,
    graph_id: &str,
    image_id: &str,
    filename: &str,
    mime_type: &str,
    pending_path: &Path,
) -> AppResult<OriginalFileManifest> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )
        .map_err(AppError::storage)?;
    adopt_pending_image_file_with_lease(app, graph_id, image_id, filename, mime_type, pending_path)
}

fn adopt_pending_image_file_with_lease(
    app: &AppHandle,
    graph_id: &str,
    image_id: &str,
    filename: &str,
    mime_type: &str,
    pending_path: &Path,
) -> AppResult<OriginalFileManifest> {
    let _flush_guard = crate::cell_durability::write_guard();
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let original_dir = image_original_dir(&graph_dir, image_id).map_err(AppError::storage)?;
    let manifest =
        save_original_file_from_path_to_dir(&original_dir, filename, mime_type, pending_path)
            .map_err(AppError::storage)?;
    remove_file_if_exists(pending_path)
        .map_err(|error| AppError::storage(format!("remove pending image file: {error}")))?;
    touch_graph_updated_at(&graph_dir).map_err(AppError::storage)?;
    Ok(manifest)
}

/// Reached from `loopback_hosted_serve_image` — including its anonymous,
/// gateway-unauthenticated, query-signed-token branch — after the caller has
/// already validated the token (or bearer scope). Uses the non-healing graph
/// lookup: a missing graph must 404, never self-heal. A self-healed (freshly
/// created, empty) graph could never contain the requested image anyway, so
/// this changes no legitimate behavior (F4c security review finding 1).
pub(super) fn read_image_file(
    app: &AppHandle,
    graph_id: &str,
    image_id: &str,
) -> AppResult<(OriginalFileManifest, Vec<u8>)> {
    let graph_dir = existing_graph_dir_no_heal(app, graph_id).map_err(AppError::storage)?;
    let original_dir = image_original_dir(&graph_dir, image_id).map_err(AppError::storage)?;
    read_original_file_from_dir(&original_dir).map_err(AppError::storage)
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod tests {
    use super::*;
    use crate::{
        document_service::{create_document, CreateDocumentInput},
        graph_service::{create_graph_service, CreateGraphInput},
    };
    use std::{sync::mpsc, time::Duration};
    #[cfg(feature = "desktop")]
    use tauri::Manager;
    use uuid::Uuid;

    #[test]
    fn direct_original_file_write_waits_for_graph_lifecycle_lease() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-original-file-lease-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "original-file-lease";
                let document_id = "original-document";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Original file lease".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                create_document(
                    app.clone(),
                    CreateDocumentInput {
                        graph_id: graph_id.to_string(),
                        title: "Original document".to_string(),
                        document_id: Some(document_id.to_string()),
                    },
                )
                .expect("create document");
                let coordinator = app.state::<
                    crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                >();
                let lease = coordinator
                    .acquire_hot_write(graph_id)
                    .await
                    .expect("hold graph lease");
                let write_app = app.clone();
                let (started_tx, started_rx) = mpsc::channel();
                let (done_tx, done_rx) = mpsc::channel();
                let writer = std::thread::spawn(move || {
                    started_tx.send(()).expect("writer started");
                    let result = save_original_file(
                        write_app,
                        SaveOriginalFileInput {
                            graph_id: graph_id.to_string(),
                            document_id: document_id.to_string(),
                            filename: "source.txt".to_string(),
                            mime_type: "text/plain".to_string(),
                            data_base64: BASE64_STANDARD.encode(b"leased original"),
                        },
                    );
                    done_tx.send(result).expect("writer result");
                });
                started_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("writer thread started");
                assert!(
                    done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                    "direct original-file write bypassed the held graph lease"
                );
                drop(lease);
                let record = done_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("writer resumed")
                    .expect("original-file write");
                assert_eq!(record.filename, "source.txt");
                writer.join().expect("writer thread");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
