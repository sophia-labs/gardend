use crate::app_runtime::AppHandle;
pub(crate) use crate::graph_record_store::{
    mutate_graph_record, mutate_graph_record_canonical, next_content_revision, read_graph_record,
    touch_graph_content_revision, touch_graph_updated_at, write_graph_record,
    write_graph_record_canonical, GraphRecord,
};
pub(crate) use crate::graph_usage_service::{hosted_graph_stats, local_graph_storage_usage};
use crate::{
    app_error::{AppError, AppResult},
    app_error_codes,
    clock::timestamp,
    graph_catalog_store::{
        delete_cached_profile_graph_record, load_cached_profile_graph_records,
        upsert_cached_profile_graph_record,
    },
    graph_duplicate_storage::{
        acquire_graph_publication_lock, ExistingTargetPolicy, GraphPublicationReservation,
    },
    graph_metadata_materializer::{
        delete_profile_graph_catalog_entry, materialize_profile_graph_catalog_entry,
        materialize_profile_metadata_graph,
    },
    ids::{make_graph_id, normalize_title, validate_local_id},
    paths::{ensure_graph_layout, graphs_dir, profile_dir},
    profile_rdf_store_service::open_profile_metadata_store,
    profile_service::{ensure_profile, touch_profile_updated_at},
    rdf_service::{open_graph_store, reconcile_graph_record},
    runtime_config::{
        GRAPH_STATUS_ACTIVE, GRAPH_STATUS_DELETED, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID,
    },
    storage::{create_dir_all, display_path, read_json},
};
use serde::Deserialize;
use std::{collections::HashMap, fs, io::ErrorKind, path::Path};
#[cfg(feature = "desktop")]
use tauri::Manager;
use uuid::Uuid;

const PROFILE_METADATA_FULL_REFRESH_GRAPH_LIMIT: usize = 32;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateGraphInput {
    pub(crate) title: String,
    #[serde(default, alias = "graph_id")]
    pub(crate) graph_id: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    /// When set, tags the resulting GraphRecord with `created_by_operation_id` for
    /// partial-replay detection (see operation_completion_ledger). Multi-step CRDT
    /// ops like `graph.importArchive` pass their journal operationId here.
    #[serde(default, alias = "operation_id")]
    pub(crate) operation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateGraphMetadataInput {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn list_graphs(app: AppHandle) -> Result<Vec<GraphRecord>, String> {
    list_graphs_service(&app).map_err(AppError::message)
}

pub(crate) fn list_graphs_service(app: &AppHandle) -> AppResult<Vec<GraphRecord>> {
    read_profile_graph_records(app, false)
}

pub(crate) fn read_profile_graph_records(
    app: &AppHandle,
    include_deleted: bool,
) -> AppResult<Vec<GraphRecord>> {
    ensure_profile(app).map_err(AppError::storage)?;
    let profile_dir = profile_dir(app).map_err(AppError::storage)?;
    let graphs_dir = graphs_dir(app).map_err(AppError::storage)?;
    create_dir_all(&graphs_dir).map_err(AppError::storage)?;

    read_manifest_graph_records(
        &profile_dir,
        &graphs_dir,
        load_cached_profile_graph_records(&profile_dir, true)?,
        include_deleted,
    )
}

/// Read canonical graph manifests and reconcile the Turso catalog projection.
///
/// Membership, lifecycle status, and ordering all come from `graph.json`.
/// Cached rows may make metadata queries cheaper elsewhere, but they must not
/// hide a newly created graph, resurrect a tombstone, or preserve a graph whose
/// manifest was removed.
fn read_manifest_graph_records(
    profile_dir: &Path,
    graphs_dir: &Path,
    cached: Vec<GraphRecord>,
    include_deleted: bool,
) -> AppResult<Vec<GraphRecord>> {
    let mut cached_by_id = cached
        .into_iter()
        .map(|graph| (graph.graph_id.clone(), graph))
        .collect::<HashMap<_, _>>();
    let mut graphs = Vec::new();
    let entries = fs::read_dir(graphs_dir)
        .map_err(|error| AppError::storage(format!("read graphs dir: {error}")))?;

    for entry in entries {
        let entry =
            entry.map_err(|error| AppError::storage(format!("read graph entry: {error}")))?;
        let entry_type = entry.file_type().map_err(|error| {
            AppError::storage(format!(
                "read graph entry type {}: {error}",
                entry.path().display()
            ))
        })?;
        if !entry_type.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("graph.json");
        let metadata = match fs::metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(AppError::storage(format!(
                    "inspect graph manifest {}: {error}",
                    manifest_path.display()
                )))
            }
        };
        if !metadata.is_file() {
            return Err(AppError::storage(format!(
                "graph manifest is not a regular file: {}",
                manifest_path.display()
            )));
        }

        let graph = read_json::<GraphRecord>(&manifest_path).map_err(AppError::storage)?;
        let directory_graph_id = entry.file_name().into_string().map_err(|_| {
            AppError::storage(format!(
                "graph directory name is not valid UTF-8: {}",
                entry.path().display()
            ))
        })?;
        if graph.graph_id != directory_graph_id {
            return Err(AppError::storage(format!(
                "graph manifest identity mismatch: directory {directory_graph_id}, record {}",
                graph.graph_id
            )));
        }

        let cache_matches = match cached_by_id.remove(&graph.graph_id) {
            Some(cached) => {
                serde_json::to_vec(&cached).map_err(|error| {
                    AppError::serialization(format!(
                        "serialize cached graph record {}: {error}",
                        graph.graph_id
                    ))
                })? == serde_json::to_vec(&graph).map_err(|error| {
                    AppError::serialization(format!(
                        "serialize canonical graph record {}: {error}",
                        graph.graph_id
                    ))
                })?
            }
            None => false,
        };
        if !cache_matches {
            upsert_cached_profile_graph_record(profile_dir, &graph)?;
        }
        if include_deleted || graph.status != GRAPH_STATUS_DELETED {
            graphs.push(graph);
        }
    }

    // Any cache row not consumed by a canonical manifest is stale. The full
    // directory scan above completed without an I/O error, so absence here is
    // a real removal rather than an inaccessible/corrupt manifest.
    for graph_id in cached_by_id.keys() {
        delete_cached_profile_graph_record(profile_dir, graph_id)?;
    }

    graphs.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.graph_id.cmp(&right.graph_id))
    });
    Ok(graphs)
}

fn refresh_profile_metadata_graph_for_record(
    app: &AppHandle,
    graph: &GraphRecord,
) -> AppResult<()> {
    let profile = ensure_profile(app).map_err(AppError::storage)?;
    let graphs = read_profile_graph_records(app, true)?;
    let store = open_profile_metadata_store(app).map_err(AppError::rdf)?;
    if graphs.len() <= PROFILE_METADATA_FULL_REFRESH_GRAPH_LIMIT {
        materialize_profile_metadata_graph(&store, &profile, &graphs).map_err(AppError::rdf)
    } else {
        let Some(canonical) = graphs.iter().find(|candidate| {
            candidate.graph_id == graph.graph_id && candidate.incarnation_id == graph.incarnation_id
        }) else {
            // The expected incarnation disappeared or was replaced before its
            // repair tail ran. Never incrementally project the stale argument.
            return Ok(());
        };
        materialize_profile_graph_catalog_entry(&store, &profile, canonical).map_err(AppError::rdf)
    }
}

fn canonical_graph_for_projection(
    app: &AppHandle,
    expected: &GraphRecord,
) -> AppResult<Option<GraphRecord>> {
    let manifest_path = graphs_dir(app)
        .map_err(AppError::storage)?
        .join(&expected.graph_id)
        .join("graph.json");
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let canonical = read_json::<GraphRecord>(&manifest_path).map_err(AppError::storage)?;
    if canonical.graph_id != expected.graph_id
        || canonical.incarnation_id != expected.incarnation_id
    {
        return Ok(None);
    }
    Ok(Some(canonical))
}

/// Repairable projections that follow canonical graph publication.
///
/// `graph.json` inside the atomically published directory is membership
/// authority. Turso/profile-RDF/timestamp failures after that boundary must not
/// report creation failure (which would invite a retry against a graph that is
/// already real); canonical reads repair the cache, and this helper retries all
/// tails best-effort immediately.
pub(crate) fn repair_published_graph_projections_best_effort(app: &AppHandle, graph: &GraphRecord) {
    let canonical = match canonical_graph_for_projection(app, graph) {
        Ok(Some(canonical)) => canonical,
        Ok(None) => {
            log::warn!(
                "Skipped projection repair for superseded graph incarnation {}",
                graph.graph_id
            );
            return;
        }
        Err(error) => {
            log::warn!(
                "Failed to reread canonical graph {} before projection repair: {error}",
                graph.graph_id
            );
            return;
        }
    };
    match profile_dir(app)
        .map_err(AppError::storage)
        .and_then(|profile_dir| upsert_cached_profile_graph_record(&profile_dir, &canonical))
    {
        Ok(()) => {}
        Err(error) => log::warn!(
            "Failed to project published graph {} into the catalog cache: {error}",
            graph.graph_id
        ),
    }
    if let Err(error) = touch_profile_updated_at(app) {
        log::warn!(
            "Failed to touch profile after publishing graph {}: {error}",
            graph.graph_id
        );
    }
    // Reread after the cache/timestamp tails. This is redundant while the
    // lifecycle lease is held, but keeps non-reentrant self-heal safe if a
    // canonical tombstone or replacement won a cross-entrypoint race.
    let rdf_record = match canonical_graph_for_projection(app, graph) {
        Ok(Some(canonical)) => canonical,
        Ok(None) => return,
        Err(error) => {
            log::warn!(
                "Failed to reread canonical graph {} before metadata projection: {error}",
                graph.graph_id
            );
            return;
        }
    };
    if let Err(error) = refresh_profile_metadata_graph_for_record(app, &rdf_record) {
        log::warn!(
            "Failed to materialize profile metadata after publishing graph {}: {error}",
            graph.graph_id
        );
    }
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn create_graph(
    app: AppHandle,
    input: CreateGraphInput,
) -> Result<GraphRecord, String> {
    create_graph_service_async(&app, input)
        .await
        .map_err(AppError::message)
}

pub(crate) fn create_graph_service(
    app: &AppHandle,
    input: CreateGraphInput,
) -> AppResult<GraphRecord> {
    let (_, graph_id) = create_graph_identity(&input)?;
    let _lease = app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
        .map(|coordinator| coordinator.acquire_lifecycle_exclusive_blocking(&graph_id))
        .transpose()
        .map_err(AppError::storage)?;
    create_graph_service_inner(app, input)
}

fn create_graph_identity(input: &CreateGraphInput) -> AppResult<(String, String)> {
    let title = normalize_title(&input.title).map_err(AppError::validation)?;
    let graph_id = match input
        .graph_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => {
            validate_local_id(value, "graph_id").map_err(AppError::validation)?;
            value.to_string()
        }
        None => make_graph_id(&title),
    };
    Ok((title, graph_id))
}

/// Shared non-reentrant create body. Public sync/async services and
/// graph.importArchive enter with the target lifecycle lease. Headless
/// websocket self-heal also calls this body because its request root already
/// owns that lease. The kernel publication lock remains the cross-process and
/// nested-publisher no-clobber boundary.
pub(crate) fn create_graph_service_inner(
    app: &AppHandle,
    input: CreateGraphInput,
) -> AppResult<GraphRecord> {
    create_graph_service_inner_with_incarnation(app, input, None)
}

fn requested_graph_incarnation(
    graph_incarnation: Option<String>,
    operation_id: Option<&str>,
) -> AppResult<Option<String>> {
    let Some(value) = graph_incarnation
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    Uuid::parse_str(value).map_err(|_| AppError::validation("graphIncarnation must be a UUID"))?;
    if operation_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(AppError::validation(
            "operationId is required with a client-minted graphIncarnation",
        ));
    }
    Ok(Some(value.to_string()))
}

fn create_graph_service_inner_with_incarnation(
    app: &AppHandle,
    input: CreateGraphInput,
    graph_incarnation: Option<String>,
) -> AppResult<GraphRecord> {
    let _durability_guard = crate::cell_durability::write_guard();
    ensure_profile(app).map_err(AppError::storage)?;
    let (title, graph_id) = create_graph_identity(&input)?;
    let profile_dir = profile_dir(app).map_err(AppError::storage)?;
    let graph_dir = graphs_dir(app).map_err(AppError::storage)?.join(&graph_id);
    let graph_incarnation =
        requested_graph_incarnation(graph_incarnation, input.operation_id.as_deref())?;
    if graph_incarnation.is_some() && graph_dir.join("graph.json").is_file() {
        let existing =
            read_json::<GraphRecord>(&graph_dir.join("graph.json")).map_err(AppError::storage)?;
        if existing.status == GRAPH_STATUS_ACTIVE
            && existing.incarnation_id.as_deref() == graph_incarnation.as_deref()
            && existing.created_by_operation_id.as_deref()
                == input
                    .operation_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
        {
            // A retry after a lost create acknowledgement converges on the
            // exact client-minted graph lifetime instead of returning 409.
            return Ok(existing);
        }
        return Err(AppError::conflict(format!(
            "graph already exists with a different lifecycle identity: {graph_id}"
        ))
        .with_code(app_error_codes::GRAPH_LIFECYCLE_IDENTITY_MISMATCH));
    }
    let now = timestamp();

    let graph = GraphRecord {
        graph_id: graph_id.clone(),
        title,
        description: input
            .description
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        status: GRAPH_STATUS_ACTIVE.to_string(),
        origin: LOCAL_GRAPH_ORIGIN.to_string(),
        provider_id: LOCAL_PROVIDER_ID.to_string(),
        local_path: display_path(&graph_dir),
        created_at: now.clone(),
        incarnation_id: Some(graph_incarnation.unwrap_or_else(|| Uuid::new_v4().to_string())),
        updated_at: now,
        capabilities: vec![
            "graph.local.open".to_string(),
            "graph.local.persist".to_string(),
            "graph.local.query.rdf".to_string(),
            "graph.local.export.rdf".to_string(),
            "document.local.persist".to_string(),
            "document.local.ydoc".to_string(),
            "workspace.local.ydoc".to_string(),
            "document.local.tiptap-tree".to_string(),
            "graph.local.semantic-index".to_string(),
        ],
        created_by_operation_id: input
            .operation_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        validation_policy: crate::runtime_config::ValidationPolicy::default(),
        // A brand-new graph has no document-side content revision yet — the seed
        // falls back to `created_at` until the first document/workspace save bumps it.
        content_revision: None,
    };

    let publication = GraphPublicationReservation::acquire(
        &profile_dir,
        graph_dir.clone(),
        &graph_id,
        ExistingTargetPolicy::AdoptUnpublishedLayout,
    )
    .map_err(|error| {
        if error == format!("graph already exists: {graph_id}") {
            AppError::conflict(error).with_code(app_error_codes::GRAPH_EXISTS)
        } else {
            AppError::storage(error)
        }
    })?;
    let stage_result = (|| -> AppResult<()> {
        ensure_graph_layout(publication.stage_dir()).map_err(AppError::storage)?;
        let store = open_graph_store(publication.stage_dir()).map_err(AppError::rdf)?;
        // Meaningful-Object engine reconcile (replaces the wholesale
        // `materialize_graph_record`). PURE PARITY: same 8-triple desired set,
        // reached by value-diff. The RDF store remains outside visible graph
        // membership until every essential file is ready.
        let _diff = reconcile_graph_record(&store, &graph).map_err(AppError::rdf)?;
        drop(store);
        crate::rdf_store_service::evict_graph_store(publication.stage_dir())
            .map_err(AppError::rdf)?;
        // The active manifest is the final staged file for a new graph and the
        // atomic in-place membership boundary for an adopted hot-sidecar
        // layout. No adopted authority is moved out of its canonical path.
        publication.publish_graph_record(&graph).map_err(|error| {
            if error == "target graph directory already exists"
                || error == "target graph manifest already exists"
            {
                AppError::conflict(format!("graph already exists: {graph_id}"))
                    .with_code(app_error_codes::GRAPH_EXISTS)
            } else {
                AppError::storage(error)
            }
        })?;
        Ok(())
    })();
    if let Err(error) = stage_result {
        let cleanup_errors = publication.cleanup_failed_stage(false);
        if cleanup_errors.is_empty() {
            return Err(error);
        }
        let message = format!(
            "{}; additionally failed cleanup: {}",
            error.message_ref(),
            cleanup_errors.join("; ")
        );
        // `with_message` preserves both `kind` and `code` — the exhaustive
        // kind re-match this replaced would otherwise drop the code silently
        // (D9's regression guard: `coded_conflict_survives_message_rewrite`).
        return Err(error.with_message(message));
    }

    publication.sync_publication_parents_best_effort();
    repair_published_graph_projections_best_effort(app, &graph);

    Ok(graph)
}

pub(crate) async fn create_graph_service_async(
    app: &AppHandle,
    input: CreateGraphInput,
) -> AppResult<GraphRecord> {
    let (_, graph_id) = create_graph_identity(&input)?;
    let _lease = match app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
    {
        Some(coordinator) => Some(
            coordinator
                .acquire_lifecycle_exclusive(&graph_id)
                .await
                .map_err(AppError::storage)?,
        ),
        None => None,
    };
    let app = app.clone();
    crate::app_runtime::async_runtime::spawn_blocking(move || create_graph_service_inner(&app, input))
        .await
        .map_err(|error| {
            AppError::internal(format!("create graph blocking task failed: {error}"))
        })?
}

/// Create a graph under a client-minted lifetime identity. This is the
/// sync-later control-plane entry: graph id + incarnation + operation id make
/// retries idempotent and make a later delete safe against same-id recreation.
pub(crate) async fn create_graph_service_async_with_incarnation(
    app: &AppHandle,
    input: CreateGraphInput,
    graph_incarnation: String,
) -> AppResult<GraphRecord> {
    let (_, graph_id) = create_graph_identity(&input)?;
    let _lease = match app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
    {
        Some(coordinator) => Some(
            coordinator
                .acquire_lifecycle_exclusive(&graph_id)
                .await
                .map_err(AppError::storage)?,
        ),
        None => None,
    };
    let app = app.clone();
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        create_graph_service_inner_with_incarnation(&app, input, Some(graph_incarnation))
    })
    .await
    .map_err(|error| AppError::internal(format!("create graph blocking task failed: {error}")))?
}

pub(crate) fn update_graph_metadata(
    app: AppHandle,
    graph_id: String,
    input: UpdateGraphMetadataInput,
) -> Result<GraphRecord, String> {
    update_graph_metadata_service(&app, graph_id, input).map_err(AppError::message)
}

pub(crate) fn update_graph_metadata_service(
    app: &AppHandle,
    graph_id: String,
    input: UpdateGraphMetadataInput,
) -> AppResult<GraphRecord> {
    let _lease = app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
        .map(|coordinator| coordinator.acquire_hot_write_blocking(&graph_id))
        .transpose()
        .map_err(AppError::storage)?;
    update_graph_metadata_service_with_lease(app, graph_id, input)
}

pub(crate) async fn update_graph_metadata_service_async(
    app: &AppHandle,
    graph_id: String,
    input: UpdateGraphMetadataInput,
) -> AppResult<GraphRecord> {
    let _lease = match app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
    {
        Some(coordinator) => Some(
            coordinator
                .acquire_hot_write(&graph_id)
                .await
                .map_err(AppError::storage)?,
        ),
        None => None,
    };
    update_graph_metadata_service_with_lease(app, graph_id, input)
}

fn update_graph_metadata_service_with_lease(
    app: &AppHandle,
    graph_id: String,
    input: UpdateGraphMetadataInput,
) -> AppResult<GraphRecord> {
    let (graph_dir, _) = read_graph_record(app, &graph_id)?;
    let title = input
        .title
        .as_deref()
        .map(crate::ids::normalize_stored_title)
        .transpose()
        .map_err(AppError::validation)?;
    let description = input.description.map(|description| {
        let description = description.trim();
        if description.is_empty() {
            None
        } else {
            Some(description.to_string())
        }
    });
    let graph = mutate_graph_record(&graph_dir, move |graph| {
        if graph.status == GRAPH_STATUS_DELETED {
            return Err(AppError::not_found(format!("graph not found: {graph_id}")));
        }
        if let Some(title) = title {
            graph.title = title;
        }
        if let Some(description) = description {
            graph.description = description;
        }
        let content_revision = next_content_revision(graph.content_revision.as_deref())?;
        graph.updated_at = content_revision.clone();
        // A graph-metadata edit changes the graph record the seed materializes —
        // bump the seed revision in the same locked read/modify/write.
        graph.content_revision = Some(content_revision);
        Ok(graph.clone())
    })?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    // Meaningful-Object engine reconcile (replaces the wholesale
    // `materialize_graph_record`). PURE PARITY with the old path; on a metadata
    // edit it emits only the changed slots (e.g. title + modified) instead of
    // tearing down and re-inserting all 8. The reconcile now returns the
    // structured `TripleDiff`; the op count is re-projected via
    // `TripleDiff::op_count` at the consumer. TODO(MO-delta-routing): route this
    // delta to the log instead of discarding it.
    let _diff = reconcile_graph_record(&store, &graph).map_err(AppError::rdf)?;
    touch_profile_updated_at(app).map_err(AppError::storage)?;
    refresh_profile_metadata_graph_for_record(app, &graph)?;
    Ok(graph)
}

pub(crate) fn soft_delete_graph(
    app: AppHandle,
    graph_id: String,
    hard: bool,
) -> Result<serde_json::Value, String> {
    soft_delete_graph_service(&app, graph_id, hard).map_err(AppError::message)
}

pub(crate) fn soft_delete_graph_service(
    app: &AppHandle,
    graph_id: String,
    hard: bool,
) -> AppResult<serde_json::Value> {
    let _lease = app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
        .map(|coordinator| coordinator.acquire_lifecycle_exclusive_blocking(&graph_id))
        .transpose()
        .map_err(AppError::storage)?;
    soft_delete_graph_service_with_lease(app, graph_id, hard)
}

pub(crate) async fn soft_delete_graph_service_async(
    app: &AppHandle,
    graph_id: String,
    hard: bool,
) -> AppResult<serde_json::Value> {
    let _lease = match app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
    {
        Some(coordinator) => Some(
            coordinator
                .acquire_lifecycle_exclusive(&graph_id)
                .await
                .map_err(AppError::storage)?,
        ),
        None => None,
    };
    soft_delete_graph_service_with_lease(app, graph_id, hard)
}

fn soft_delete_graph_service_with_lease(
    app: &AppHandle,
    graph_id: String,
    hard: bool,
) -> AppResult<serde_json::Value> {
    let _durability_guard = crate::cell_durability::write_guard();
    let hard_profile_dir = if hard {
        Some(profile_dir(app).map_err(AppError::storage)?)
    } else {
        None
    };
    let _publication_lock = hard_profile_dir
        .as_ref()
        .map(|profile| acquire_graph_publication_lock(profile, &graph_id))
        .transpose()
        .map_err(AppError::storage)?;
    let (graph_dir, existing_graph) = if hard {
        read_graph_record_including_deleted(
            app,
            hard_profile_dir.as_ref().expect("hard profile dir"),
            &graph_id,
        )?
    } else {
        read_graph_record(app, &graph_id)?
    };
    // `graph.json` is the canonical lifecycle boundary. A following Turso
    // catalog-cache projection may fail even though this tombstone is already
    // durable. Serialize this RMW with direct RDF/document revision writers,
    // but deliberately leave the derived cache outside the canonical commit.
    let graph = if existing_graph.status == GRAPH_STATUS_DELETED {
        existing_graph
    } else {
        mutate_graph_record_canonical(&graph_dir, |graph| {
            graph.status = GRAPH_STATUS_DELETED.to_string();
            let now = crate::clock::epoch_millis();
            let previous = crate::clock::parse_timestamp(&graph.updated_at).unwrap_or(0);
            let content_revision = graph
                .content_revision
                .as_deref()
                .and_then(crate::clock::parse_timestamp)
                .unwrap_or(0);
            graph.updated_at = now.max(previous).max(content_revision).to_string();
            Ok(graph.clone())
        })?
    };
    let cache_result = if hard {
        delete_cached_profile_graph_record(
            hard_profile_dir.as_ref().expect("hard profile dir"),
            &graph_id,
        )
    } else {
        profile_dir(app)
            .map_err(AppError::storage)
            .and_then(|profile| upsert_cached_profile_graph_record(&profile, &graph))
    };
    if hard {
        return finish_hard_delete_after_canonical_tombstone(
            app,
            hard_profile_dir.as_ref().expect("hard profile dir"),
            &graph_dir,
            graph_id,
            graph,
            cache_result,
        );
    }
    finish_soft_delete_after_canonical_tombstone(
        app,
        &graph_dir,
        graph_id,
        hard,
        graph,
        cache_result,
    )
}

/// Controlled test seam: execute the real lifecycle bodies while the test
/// holds their existing exclusive lease, so waiting public admissions resume
/// only after an actual hard delete and same-ID recreation.
#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
pub(crate) fn replace_graph_under_test_lease(
    app: &AppHandle,
    graph_id: &str,
    _lease: &crate::crdt_engine::persistence_coordinator::ExclusiveLease,
) -> AppResult<GraphRecord> {
    soft_delete_graph_service_with_lease(app, graph_id.to_string(), true)?;
    create_graph_service_inner(app, CreateGraphInput {
        graph_id: Some(graph_id.to_string()), title: "Successor fixture graph".into(),
        description: None, operation_id: None,
    })
}

fn read_graph_record_including_deleted(
    app: &AppHandle,
    profile_dir: &Path,
    graph_id: &str,
) -> AppResult<(std::path::PathBuf, GraphRecord)> {
    validate_local_id(graph_id, "graph_id").map_err(AppError::validation)?;
    let canonical_dir = graphs_dir(app).map_err(AppError::storage)?.join(graph_id);
    let trash_dir = profile_dir.join(".graph-trash").join(graph_id);
    let graph_dir = if canonical_dir.join("graph.json").is_file() {
        canonical_dir
    } else {
        trash_dir
    };
    let manifest = graph_dir.join("graph.json");
    if !manifest.is_file() {
        return Err(AppError::not_found(format!("graph not found: {graph_id}")));
    }
    let graph = read_json::<GraphRecord>(&manifest).map_err(AppError::storage)?;
    if graph.graph_id != graph_id {
        return Err(AppError::storage(format!(
            "graph manifest identity mismatch: directory {graph_id}, record {}",
            graph.graph_id
        )));
    }
    Ok((graph_dir, graph))
}

fn finish_hard_delete_after_canonical_tombstone(
    app: &AppHandle,
    profile_dir: &Path,
    graph_dir: &Path,
    graph_id: String,
    graph: GraphRecord,
    cache_result: AppResult<()>,
) -> AppResult<serde_json::Value> {
    let generation_result = if let Some(coordinator) =
        app.try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
    {
        coordinator
            .advance_generation(&graph_id)
            .map(|_| ())
            .map_err(AppError::storage)
    } else {
        Ok(())
    };
    if let Some(registry) = app.try_state::<crate::crdt_engine::rooms::RoomRegistry>() {
        registry.evict_graph(&graph_id);
    }
    let rdf_cache_result =
        crate::rdf_store_service::evict_graph_store(graph_dir).map_err(AppError::rdf);
    let purge_result =
        purge_hard_deleted_graph(profile_dir, graph_dir, &graph_id).map_err(AppError::storage);
    let purged = purge_result.is_ok();
    let profile_result = touch_profile_updated_at(app).map_err(AppError::storage);
    let projection_result = (|| {
        let profile = ensure_profile(app).map_err(AppError::storage)?;
        let store = open_profile_metadata_store(app).map_err(AppError::rdf)?;
        delete_profile_graph_catalog_entry(&store, &profile, &graph_id).map_err(AppError::rdf)
    })();

    // The canonical tombstone is the delete commit point. Once the directory
    // is moved out of graphs/, same-ID creation is safe even if a later hidden
    // trash cleanup or derived projection tail needs repair.
    for (tail, result) in [
        ("catalog cache removal", cache_result),
        ("lifecycle generation", generation_result),
        ("RDF store cache eviction", rdf_cache_result),
        ("physical graph purge", purge_result),
        ("profile timestamp", profile_result),
        ("profile metadata RDF removal", projection_result),
    ] {
        if let Err(error) = result {
            log::warn!(
                "Graph {} is canonically hard-deleted; repairable {tail} tail failed: {error}",
                graph.graph_id
            );
        }
    }
    Ok(serde_json::json!({
        "graph_id": graph_id,
        "graphId": graph.graph_id,
        "status": GRAPH_STATUS_DELETED,
        "hard": true,
        "purged": purged,
        "hardCleanupPending": !purged,
    }))
}

fn purge_hard_deleted_graph(
    profile_dir: &Path,
    graph_dir: &Path,
    graph_id: &str,
) -> Result<(), String> {
    let trash_root = profile_dir.join(".graph-trash");
    create_dir_all(&trash_root)?;
    let trash_dir = trash_root.join(graph_id);
    if graph_dir != trash_dir {
        if trash_dir.exists() {
            crate::storage::remove_dir_all(&trash_dir).map_err(|error| {
                format!(
                    "remove prior hard-delete trash {}: {error}",
                    display_path(&trash_dir)
                )
            })?;
        }
        fs::rename(graph_dir, &trash_dir).map_err(|error| {
            format!(
                "move hard-deleted graph {} to {}: {error}",
                display_path(graph_dir),
                display_path(&trash_dir)
            )
        })?;
        if let Some(graphs_parent) = graph_dir.parent() {
            if let Err(error) = crate::storage_atomic::sync_parent_dir(graphs_parent) {
                log::warn!(
                    "Hard-deleted graph {graph_id} moved out of publication but graphs parent sync failed: {error}"
                );
            }
        }
    }
    crate::storage::remove_dir_all(&trash_dir).map_err(|error| {
        format!(
            "remove hard-deleted graph trash {}: {error}",
            display_path(&trash_dir)
        )
    })?;
    if let Err(error) = crate::storage_atomic::sync_parent_dir(&trash_root) {
        log::warn!("Hard-delete trash removal for {graph_id} was not directory-synced: {error}");
    }
    Ok(())
}

fn finish_soft_delete_after_canonical_tombstone(
    app: &AppHandle,
    graph_dir: &Path,
    graph_id: String,
    hard: bool,
    graph: GraphRecord,
    cache_result: AppResult<()>,
) -> AppResult<serde_json::Value> {
    let generation_result = if let Some(coordinator) =
        app.try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
    {
        coordinator
            .advance_generation(&graph_id)
            .map(|_| ())
            .map_err(AppError::storage)
    } else {
        Ok(())
    };
    // Deletion is also a live-room lifecycle boundary. Remove the graph's
    // hosted rooms synchronously so existing websocket Arcs reject later
    // updates and pending debounce/retry generations cannot become zombies.
    if let Some(registry) = app.try_state::<crate::crdt_engine::rooms::RoomRegistry>() {
        registry.evict_graph(&graph_id);
    }
    let rdf_cache_result =
        crate::rdf_store_service::evict_graph_store(graph_dir).map_err(AppError::rdf);
    // Evaluate every post-tombstone tail, but never report the canonical delete
    // as failed. Canonical reads repair the catalog/profile projections later;
    // lifecycle generation and cache eviction are retried on a future delete or
    // process restart and are logged loudly here.
    let profile_result = touch_profile_updated_at(app).map_err(AppError::storage);
    let projection_result = refresh_profile_metadata_graph_for_record(app, &graph);
    for (tail, result) in [
        ("catalog cache", cache_result),
        ("lifecycle generation", generation_result),
        ("RDF store cache eviction", rdf_cache_result),
        ("profile timestamp", profile_result),
        ("profile metadata RDF", projection_result),
    ] {
        if let Err(error) = result {
            log::warn!(
                "Graph {} is canonically deleted; repairable {tail} tail failed: {error}",
                graph.graph_id
            );
        }
    }
    Ok(serde_json::json!({
        "graph_id": graph_id,
        "graphId": graph.graph_id,
        "status": GRAPH_STATUS_DELETED,
        "hard": hard,
    }))
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod lifecycle_serialization_tests {
    use super::*;
    use std::path::PathBuf;
    #[cfg(feature = "desktop")]
    use tauri::Manager;
    use uuid::Uuid;

    fn temp_profile() -> PathBuf {
        std::env::temp_dir().join(format!("garden-graph-lifecycle-{}", Uuid::new_v4()))
    }

    #[test]
    fn same_millisecond_metadata_writes_share_the_monotonic_revision_helper() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "metadata-monotonic";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Metadata Monotonic".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let (graph_dir, mut graph) = read_graph_record(&app, graph_id).expect("graph record");
            let pinned = crate::clock::epoch_millis() + 60_000;
            graph.updated_at = pinned.to_string();
            graph.content_revision = Some(pinned.to_string());
            write_graph_record(&graph_dir, &graph).expect("pin future revision");

            let first = update_graph_metadata_service(
                &app,
                graph_id.to_string(),
                UpdateGraphMetadataInput {
                    title: Some("First metadata write".to_string()),
                    description: None,
                },
            )
            .expect("first metadata update");
            let second = update_graph_metadata_service(
                &app,
                graph_id.to_string(),
                UpdateGraphMetadataInput {
                    title: Some("Second metadata write".to_string()),
                    description: None,
                },
            )
            .expect("second metadata update");
            let first_revision = first
                .content_revision
                .expect("first content revision")
                .parse::<u128>()
                .expect("numeric first revision");
            let second_revision = second
                .content_revision
                .expect("second content revision")
                .parse::<u128>()
                .expect("numeric second revision");
            assert_eq!(first_revision, pinned + 1);
            assert_eq!(second_revision, pinned + 2);
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn metadata_waiter_cannot_rewrite_deleted_graph_from_stale_record() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "metadata-delete-race";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Original title".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let (graph_dir, _) = read_graph_record(&app, graph_id).expect("active graph");
                let coordinator = app.state::<
                    crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                >();
                let lifecycle_lease = coordinator
                    .acquire_lifecycle_exclusive(graph_id)
                    .await
                    .expect("graph lease");

                let app_for_update = app.clone();
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let update = crate::app_runtime::async_runtime::spawn(async move {
                    let _ = started_tx.send(());
                    update_graph_metadata_service_async(
                        &app_for_update,
                        graph_id.to_string(),
                        UpdateGraphMetadataInput {
                            title: Some("Stale rewrite".to_string()),
                            description: None,
                        },
                    )
                    .await
                });
                started_rx.await.expect("metadata waiter started");
                tokio::task::yield_now().await;

                soft_delete_graph_service_with_lease(&app, graph_id.to_string(), false)
                    .expect("delete while lifecycle lease is held");
                drop(lifecycle_lease);
                let error = update
                    .await
                    .expect("metadata task")
                    .expect_err("metadata update sees deleted graph after acquiring lease");
                assert_eq!(error.kind(), crate::app_error::AppErrorKind::NotFound);

                let stored: GraphRecord =
                    read_json(&graph_dir.join("graph.json")).expect("raw deleted record");
                assert_eq!(stored.status, GRAPH_STATUS_DELETED);
                assert_eq!(stored.title, "Original title");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn canonical_catalog_discovers_missing_rows_repairs_tombstones_and_resorts() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Catalog Sentinel".to_string(),
                    graph_id: Some("catalog-sentinel".to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create cached sentinel");
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Catalog Target".to_string(),
                    graph_id: Some("catalog-target".to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create target");

            let (target_dir, mut canonical) =
                read_graph_record(&app, "catalog-target").expect("active target");
            canonical.updated_at = "9999-12-31T23:59:59.000Z".to_string();
            write_graph_record_canonical(&target_dir, &canonical)
                .expect("advance only the canonical manifest");
            delete_cached_profile_graph_record(&profile, "catalog-target")
                .expect("remove target cache row");

            let discovered = list_graphs_service(&app).expect("scan canonical manifests");
            assert_eq!(
                discovered
                    .iter()
                    .map(|graph| graph.graph_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["catalog-target", "catalog-sentinel"],
                "an existing cache row must not hide a different uncached manifest"
            );

            let mut stale = canonical.clone();
            stale.updated_at = "0000-01-01T00:00:00.000Z".to_string();
            upsert_cached_profile_graph_record(&profile, &stale)
                .expect("seed stale ordering projection");
            let resorted = list_graphs_service(&app).expect("replace and resort stale cache row");
            assert_eq!(resorted[0].graph_id, "catalog-target");
            assert_eq!(resorted[0].updated_at, canonical.updated_at);

            canonical.status = GRAPH_STATUS_DELETED.to_string();
            write_graph_record_canonical(&target_dir, &canonical)
                .expect("write canonical tombstone");
            upsert_cached_profile_graph_record(&profile, &stale)
                .expect("seed stale active cache row");

            let active = list_graphs_service(&app).expect("list canonical active manifests");
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].graph_id, "catalog-sentinel");

            upsert_cached_profile_graph_record(&profile, &stale)
                .expect("re-seed stale row for independent include-deleted repair");
            let all = read_profile_graph_records(&app, true)
                .expect("include canonical tombstones after cache repair");
            let deleted = all
                .iter()
                .find(|graph| graph.graph_id == "catalog-target")
                .expect("deleted target remains available to include-deleted reads");
            assert_eq!(deleted.status, GRAPH_STATUS_DELETED);
            let cached = load_cached_profile_graph_records(&profile, true)
                .expect("read repaired catalog projection");
            let repaired = cached
                .iter()
                .find(|graph| graph.graph_id == "catalog-target")
                .expect("target cache row");
            assert_eq!(repaired.status, GRAPH_STATUS_DELETED);
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn corrupt_manifest_type_errors_without_deleting_cached_row() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Corrupt Manifest".to_string(),
                    graph_id: Some("corrupt-manifest".to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let (graph_dir, _) = read_graph_record(&app, "corrupt-manifest").expect("active graph");
            std::fs::remove_file(graph_dir.join("graph.json")).expect("remove manifest file");
            std::fs::create_dir(graph_dir.join("graph.json"))
                .expect("replace manifest with directory");

            let error = list_graphs_service(&app)
                .expect_err("a non-regular canonical manifest is corruption, not deletion");
            assert!(error.to_string().contains("not a regular file"), "{error}");
            let cached = load_cached_profile_graph_records(&profile, true)
                .expect("cache remains readable after canonical I/O error");
            assert!(cached
                .iter()
                .any(|graph| graph.graph_id == "corrupt-manifest"));
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn canonical_tombstone_commits_and_evicts_rooms_even_when_cache_projection_fails() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "delete-cache-failure";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Delete Cache Failure".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                let (graph_dir, mut graph) =
                    read_graph_record(&app, graph_id).expect("active graph");
                let registry = app.state::<crate::crdt_engine::rooms::RoomRegistry>();
                let room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("hosted room");
                let coordinator = app.state::<
                    crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                >();
                assert_eq!(coordinator.generation(graph_id).expect("generation"), 0);

                graph.status = GRAPH_STATUS_DELETED.to_string();
                graph.updated_at = timestamp();
                write_graph_record_canonical(&graph_dir, &graph)
                    .expect("canonical tombstone is durable");
                let outcome = finish_soft_delete_after_canonical_tombstone(
                    &app,
                    &graph_dir,
                    graph_id.to_string(),
                    false,
                    graph,
                    Err(AppError::database("injected catalog cache failure")),
                )
                .expect("canonical deletion remains committed success");
                assert_eq!(outcome["status"], GRAPH_STATUS_DELETED);

                assert_eq!(coordinator.generation(graph_id).expect("generation"), 1);
                assert!(registry
                    .peek(&format!("workspace:{graph_id}"))
                    .await
                    .is_none());
                let room_error = room
                    .update_doc(|_doc, _txn| Ok(()))
                    .await
                    .expect_err("existing room Arc is evicted");
                assert!(room_error.contains("room was evicted"), "{room_error}");
                let stored: GraphRecord =
                    read_json(&graph_dir.join("graph.json")).expect("raw deleted record");
                assert_eq!(stored.status, GRAPH_STATUS_DELETED);
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn hard_delete_purges_storage_and_allows_fresh_same_id_incarnation() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "hard-delete-recreate";
                let original = create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Original incarnation".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create original graph");
                let original_incarnation = original
                    .incarnation_id
                    .clone()
                    .expect("original incarnation id");
                let (graph_dir, _) = read_graph_record(&app, graph_id).expect("original graph");
                std::fs::write(graph_dir.join("old-incarnation-sentinel"), b"old")
                    .expect("write old incarnation sentinel");
                let registry = app.state::<crate::crdt_engine::rooms::RoomRegistry>();
                let old_room = registry
                    .get_or_create(
                        &format!("workspace:{graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir),
                    )
                    .await
                    .expect("old workspace room");

                let outcome = soft_delete_graph_service(&app, graph_id.to_string(), true)
                    .expect("hard delete graph");
                assert_eq!(outcome["hard"], true);
                assert_eq!(outcome["purged"], true);
                assert_eq!(outcome["hardCleanupPending"], false);
                assert!(
                    !graph_dir.exists(),
                    "canonical graph directory survived hard delete"
                );
                assert!(!profile.join(".graph-trash").join(graph_id).exists());
                assert!(read_graph_record(&app, graph_id).is_err());
                assert!(list_graphs_service(&app)
                    .expect("list after hard delete")
                    .iter()
                    .all(|graph| graph.graph_id != graph_id));
                let old_room_error = old_room
                    .update_doc(|_doc, _txn| Ok(()))
                    .await
                    .expect_err("old room arc must remain evicted");
                assert!(
                    old_room_error.contains("room was evicted"),
                    "{old_room_error}"
                );

                let replacement = create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Replacement incarnation".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("recreate same graph id");
                assert_ne!(
                    replacement.incarnation_id.as_deref(),
                    Some(original_incarnation.as_str())
                );
                let (replacement_dir, stored) =
                    read_graph_record(&app, graph_id).expect("replacement graph");
                assert_eq!(stored.title, "Replacement incarnation");
                assert!(!replacement_dir.join("old-incarnation-sentinel").exists());
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn hard_delete_can_finish_an_existing_soft_tombstone() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "soft-then-hard-delete";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Soft then hard".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = graphs_dir(&app).expect("graphs dir").join(graph_id);
            soft_delete_graph_service(&app, graph_id.to_string(), false)
                .expect("soft delete graph");
            assert!(
                graph_dir.is_dir(),
                "soft delete unexpectedly purged storage"
            );

            let outcome = soft_delete_graph_service(&app, graph_id.to_string(), true)
                .expect("upgrade soft tombstone to hard delete");
            assert_eq!(outcome["purged"], true);
            assert!(!graph_dir.exists());
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Recreated after soft-hard".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("recreate after hard delete");
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn tauri_create_command_waits_for_the_graph_lifecycle_lease() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "tauri-create-lease";
                let coordinator = app.state::<
                    crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                >();
                let lease = coordinator
                    .acquire_hot_write(graph_id)
                    .await
                    .expect("hold create lease");
                let command_app = app.clone();
                let (done_tx, mut done_rx) = tokio::sync::mpsc::channel(1);
                let command = crate::app_runtime::async_runtime::spawn(async move {
                    let outcome = create_graph(
                        command_app,
                        CreateGraphInput {
                            title: "Leased Tauri create".to_string(),
                            graph_id: Some(graph_id.to_string()),
                            description: None,
                            operation_id: None,
                        },
                    )
                    .await;
                    let _ = done_tx.send(outcome).await;
                });

                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(30), done_rx.recv(),)
                        .await
                        .is_err(),
                    "external Tauri create bypassed the held lifecycle lease"
                );
                assert!(!graphs_dir(&app)
                    .expect("graphs dir")
                    .join(graph_id)
                    .join("graph.json")
                    .exists());

                drop(lease);
                let created =
                    tokio::time::timeout(std::time::Duration::from_secs(30), done_rx.recv())
                        .await
                        .expect("create resumed after lease release")
                        .expect("create result sent")
                        .expect("create command succeeded");
                assert_eq!(created.graph_id, graph_id);
                command.await.expect("create command task");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn stale_active_publish_tail_projects_the_canonical_tombstone_above_full_refresh_limit() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile_dir_path = temp_profile();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile_dir_path);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "stale-publish-tail";
            let active = create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Stale publish tail".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create target graph");
            let graph_root = graphs_dir(&app).expect("graphs dir");
            let target_dir = graph_root.join(graph_id);

            // Force the incremental (>32 records) profile-RDF branch without
            // paying the cost of 32 independent create pipelines.
            for index in 0..32 {
                let sibling_id = format!("projection-sibling-{index:02}");
                let sibling_dir = graph_root.join(&sibling_id);
                create_dir_all(&sibling_dir).expect("create sibling dir");
                let mut sibling = active.clone();
                sibling.graph_id = sibling_id;
                sibling.title = format!("Projection sibling {index}");
                sibling.local_path = display_path(&sibling_dir);
                sibling.incarnation_id = Some(Uuid::new_v4().to_string());
                write_graph_record_canonical(&sibling_dir, &sibling)
                    .expect("write sibling canonical record");
            }

            let mut tombstone = active.clone();
            tombstone.status = GRAPH_STATUS_DELETED.to_string();
            tombstone.updated_at = timestamp();
            write_graph_record_canonical(&target_dir, &tombstone)
                .expect("commit racing canonical tombstone");
            upsert_cached_profile_graph_record(&profile_dir_path, &active)
                .expect("seed stale active cache row");

            repair_published_graph_projections_best_effort(&app, &active);

            let cached = load_cached_profile_graph_records(&profile_dir_path, true)
                .expect("read repaired cache");
            assert_eq!(
                cached
                    .iter()
                    .find(|graph| graph.graph_id == graph_id)
                    .expect("target cache row")
                    .status,
                GRAPH_STATUS_DELETED
            );

            let profile_manifest = ensure_profile(&app).expect("profile manifest");
            let metadata_graph = crate::graph_metadata_materializer::profile_metadata_graph_iri(
                &profile_manifest.profile_id,
            );
            let subject = crate::graph_metadata_materializer::profile_graph_catalog_subject(
                &profile_manifest.profile_id,
                graph_id,
            );
            let store = open_profile_metadata_store(&app).expect("profile RDF store");
            let query = format!(
                "ASK WHERE {{ GRAPH <{metadata_graph}> {{ <{subject}> <{}status> \"deleted\" }} }}",
                crate::runtime_config::MNEMO_NS,
            );
            let result = oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(&query)
                .expect("parse status ASK")
                .on_store(&store)
                .execute()
                .expect("execute status ASK");
            assert!(matches!(
                result,
                oxigraph::sparql::QueryResults::Boolean(true)
            ));
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile_dir_path);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
