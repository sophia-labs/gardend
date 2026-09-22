use crate::{
    graph_paths::{artifacts_dir, documents_dir, images_dir},
    ids::validate_local_id,
    storage::create_dir_all,
    ydoc_paths::document_ydoc_dir,
};
use std::path::{Path, PathBuf};
#[cfg(all(feature = "headless", not(feature = "desktop")))]
use std::sync::{Mutex, OnceLock};

/// This is the raw, non-healing lookup — the F4c-era counterpart on the
/// document side of `graph_paths::existing_graph_dir_no_heal`. It stays
/// exactly this simple on purpose: history mutations, time-travel restore,
/// original-file/image access, and artifact ingest all resolve documents
/// through this function, and none of them may self-heal a missing manifest
/// — a missing document there must keep 404ing. Read-only history has a
/// separate, non-healing exception in `document_history_service` for an
/// identity-validated retained index behind an explicit deletion tombstone.
/// This lookup and every mutation still require a current manifest.
/// F8 self-heal lives only at
/// the two authoring entry points that call it and then decide for
/// themselves what to do with a miss: `document_service::read_document` and
/// `crdt_engine::block_ops::document_context`. See
/// `self_heal_missing_document` below for the healing logic itself, and F4c
/// security review finding 1 for the identical reasoning on the graph side.
pub(crate) fn existing_document_dir(
    graph_dir: &Path,
    document_id: &str,
) -> Result<PathBuf, String> {
    let document_dir = document_dir(graph_dir, document_id)?;
    if !document_dir.join("document.json").is_file() {
        return Err(format!("document not found: {document_id}"));
    }
    Ok(document_dir)
}

pub(crate) fn document_dir(graph_dir: &Path, document_id: &str) -> Result<PathBuf, String> {
    validate_local_id(document_id, "document_id")?;
    Ok(documents_dir(graph_dir).join(document_id))
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
static DOCUMENT_SELF_HEAL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Self-heal a missing `document.json` for a genuine ghost document (F8):
/// one the workspace Y.Doc already lists (a `workspace.createDocument` that
/// landed, or a block/document write that got as far as mutating the room
/// but crashed before `save_document` persisted the manifest) but which has
/// no manifest on disk. Compiled only into a pure headless (cell) binary —
/// `headless` without `desktop`, same composability rule as F4c — and only
/// acts when the gateway has told us we're a gateway-fronted cell
/// (`GARDEN_SELF_HEAL_GRAPHS=1`) — see `runtime_config::self_heal_graphs_enabled()`,
/// the exact same knob F4c gates on. No-op if the document already exists
/// (including a concurrent racer having just healed it).
///
/// Callers must check `!document.json.is_file()` themselves before invoking
/// this — see `document_service::read_document` and
/// `crdt_engine::block_ops::document_context`. Never call this from
/// `existing_document_dir` itself — see the doc comment there.
///
/// Fires ONLY for a genuine ghost: `document_id` must already be listed in
/// the persisted `workspace.json` snapshot (`workspace_snapshot_document_title`
/// below). A `document_id` the workspace has never heard of — a typo'd read
/// or mutation — is left to fail not-found, exactly as before F8.
#[cfg(all(feature = "headless", not(feature = "desktop")))]
pub(crate) fn self_heal_missing_document(
    app: &crate::app_runtime::AppHandle,
    graph_dir: &Path,
    graph_id: &str,
    document_id: &str,
) -> Result<(), String> {
    if !crate::runtime_config::self_heal_graphs_enabled() {
        return Err(format!("document not found: {document_id}"));
    }
    // Preserve the graph -> self-heal lock order used by mutation handlers.
    // Acquiring the graph lease first prevents a read-side healer and an
    // already-leased block mutation from deadlocking each other.
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )?;
    self_heal_missing_document_with_lease(app, graph_dir, graph_id, document_id)
}

/// Non-reentrant F8 healing body for mutation handlers that already own the
/// graph persistence lease.
#[cfg(all(feature = "headless", not(feature = "desktop")))]
pub(crate) fn self_heal_missing_document_with_lease(
    app: &crate::app_runtime::AppHandle,
    graph_dir: &Path,
    graph_id: &str,
    document_id: &str,
) -> Result<(), String> {
    crate::document_body_availability::require_available(graph_dir, document_id)?;
    // Both callers gate this body through `self_heal_graphs_enabled()` before
    // entry. Keeping the non-reentrant body free of a second process-global
    // flag read also makes its already-leased contract directly testable.
    let _guard = DOCUMENT_SELF_HEAL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "document self-heal lock poisoned".to_string())?;

    // Re-check under the lock: a racing request may have already healed it.
    if document_dir(graph_dir, document_id)?
        .join("document.json")
        .is_file()
    {
        return Ok(());
    }

    let title = workspace_snapshot_document_title(graph_dir, document_id)
        .ok_or_else(|| format!("document not found: {document_id}"))?;

    log::warn!(
        "F8 self-heal: creating missing document record for {document_id} in graph {graph_id}"
    );
    // create_document is idempotent — if a racer created the manifest between
    // our check above and here, it just returns the existing record.
    crate::document_service::create_document_with_lease(
        app.clone(),
        crate::document_types::CreateDocumentInput {
            graph_id: graph_id.to_string(),
            title,
            document_id: Some(document_id.to_string()),
        },
    )
    .map(|_| ())
}

/// The title the workspace Y.Doc has recorded for `document_id`, read from
/// the persisted `workspace.json` snapshot — kept fresh on every workspace
/// mutation (`crdt_engine::workspace_ops::persist_workspace`) and on every
/// block mutation's `document.write`-adjacent save. Falls back to
/// `document_id` when the entry is listed but carries no (or a blank)
/// title. Returns `None` when the snapshot is missing/unreadable or simply
/// doesn't list `document_id` — the signal that distinguishes a genuine
/// ghost from a typo'd id.
#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn workspace_snapshot_document_title(graph_dir: &Path, document_id: &str) -> Option<String> {
    let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(graph_dir);
    let snapshot: serde_json::Value = crate::storage::read_json(&snapshot_path).ok()?;
    let entry = snapshot
        .get("documents")?
        .as_array()?
        .iter()
        .find(|doc| doc.get("id").and_then(serde_json::Value::as_str) == Some(document_id))?;
    let title = entry
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| document_id.to_string());
    Some(title)
}

pub(crate) fn artifact_original_dir(
    graph_dir: &Path,
    artifact_id: &str,
) -> Result<PathBuf, String> {
    validate_local_id(artifact_id, "artifact_id")?;
    Ok(artifacts_dir(graph_dir).join(artifact_id).join("original"))
}

pub(crate) fn image_original_dir(graph_dir: &Path, image_id: &str) -> Result<PathBuf, String> {
    validate_local_id(image_id, "image_id")?;
    Ok(images_dir(graph_dir).join(image_id).join("original"))
}

pub(crate) fn ensure_document_dirs(
    graph_dir: &Path,
    document_id: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let document_dir = document_dir(graph_dir, document_id)?;
    let ydoc_dir = document_ydoc_dir(graph_dir, document_id);
    create_dir_all(&document_dir)?;
    create_dir_all(&ydoc_dir)?;
    Ok((document_dir, ydoc_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        graph_paths::documents_dir,
        ydoc_paths::{document_ydoc_state_path, workspace_snapshot_path},
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_graph_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-paths-{name}-{suffix}"))
    }

    #[test]
    fn graph_relative_paths_match_profile_layout() {
        let graph_dir = PathBuf::from("/tmp/mnemosyne-graph");
        assert_eq!(documents_dir(&graph_dir), graph_dir.join("documents"));
        assert_eq!(
            document_dir(&graph_dir, "doc-one").expect("document dir"),
            graph_dir.join("documents/doc-one")
        );
        assert_eq!(
            document_ydoc_state_path(&graph_dir, "doc-one"),
            graph_dir.join("ydocs/documents/doc-one/update-v1.bin")
        );
        assert_eq!(
            workspace_snapshot_path(&graph_dir),
            graph_dir.join("ydocs/workspace/workspace.json")
        );
    }

    #[test]
    fn ensure_document_dirs_creates_manifest_and_ydoc_directories() {
        let graph_dir = temp_graph_dir("document-layout");

        let (document_dir, ydoc_dir) =
            ensure_document_dirs(&graph_dir, "doc-one").expect("ensure document dirs");

        assert_eq!(document_dir, graph_dir.join("documents/doc-one"));
        assert_eq!(ydoc_dir, graph_dir.join("ydocs/documents/doc-one"));
        assert!(document_dir.is_dir());
        assert!(ydoc_dir.is_dir());
        let _ = fs::remove_dir_all(graph_dir);
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    mod self_heal {
        use super::*;
        use crate::storage::write_json;
        #[cfg(feature = "desktop")]
        use tauri::Manager;

        fn write_snapshot(graph_dir: &Path, documents: serde_json::Value) {
            fs::create_dir_all(workspace_snapshot_path(graph_dir).parent().unwrap()).unwrap();
            write_json(
                &workspace_snapshot_path(graph_dir),
                &serde_json::json!({ "documents": documents }),
            )
            .unwrap();
        }

        #[test]
        fn ghost_title_sourced_from_workspace_snapshot() {
            let graph_dir = temp_graph_dir("ghost-title-present");
            write_snapshot(
                &graph_dir,
                serde_json::json!([{ "id": "doc-one", "title": "Ghost Title" }]),
            );

            assert_eq!(
                workspace_snapshot_document_title(&graph_dir, "doc-one"),
                Some("Ghost Title".to_string())
            );
            let _ = fs::remove_dir_all(graph_dir);
        }

        #[test]
        fn ghost_title_falls_back_to_document_id_when_blank() {
            let graph_dir = temp_graph_dir("ghost-title-blank");
            write_snapshot(
                &graph_dir,
                serde_json::json!([{ "id": "doc-one", "title": "   " }]),
            );

            assert_eq!(
                workspace_snapshot_document_title(&graph_dir, "doc-one"),
                Some("doc-one".to_string())
            );
            let _ = fs::remove_dir_all(graph_dir);
        }

        #[test]
        fn not_a_ghost_when_unlisted_in_workspace_snapshot() {
            let graph_dir = temp_graph_dir("ghost-title-unlisted");
            write_snapshot(
                &graph_dir,
                serde_json::json!([{ "id": "doc-other", "title": "Other" }]),
            );

            assert_eq!(
                workspace_snapshot_document_title(&graph_dir, "doc-typo"),
                None
            );
            let _ = fs::remove_dir_all(graph_dir);
        }

        #[test]
        fn not_a_ghost_when_no_snapshot_exists() {
            let graph_dir = temp_graph_dir("ghost-title-no-snapshot");
            assert_eq!(
                workspace_snapshot_document_title(&graph_dir, "doc-one"),
                None
            );
        }

        #[test]
        fn already_leased_heal_uses_the_non_reentrant_create_body() {
            let _serial = crate::tauri_runtime::profile_env_serial()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let profile = temp_graph_dir("already-leased-heal-profile");
            let previous_profile = std::env::var_os("GARDEN_PROFILE_DIR");
            std::env::set_var("GARDEN_PROFILE_DIR", &profile);

            let result = std::panic::catch_unwind(|| {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "already-leased-ghost-graph";
                let document_id = "already-leased-ghost-document";
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        title: "Already Leased Ghost".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("existing graph directory");
                write_snapshot(
                    &graph_dir,
                    serde_json::json!([{ "id": document_id, "title": "Recovered Ghost" }]),
                );

                let coordinator = app.state::<
                    crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                >();
                let _lease = coordinator
                    .acquire_hot_write_blocking(graph_id)
                    .expect("acquire the executor-style graph lease");
                self_heal_missing_document_with_lease(&app, &graph_dir, graph_id, document_id)
                    .expect("heal without reacquiring the held graph lease");
                assert!(
                    document_dir(&graph_dir, document_id)
                        .expect("document directory")
                        .join("document.json")
                        .is_file(),
                    "the already-leased healer did not publish the missing manifest"
                );
            });

            match previous_profile {
                Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
                None => std::env::remove_var("GARDEN_PROFILE_DIR"),
            }
            let _ = fs::remove_dir_all(&profile);
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
        }
    }
}
