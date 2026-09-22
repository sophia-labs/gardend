use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    crdt_queue::CrdtOperation,
    document_service::{read_graph_documents_cold, read_workspace_record},
    graph_catalog_store::delete_cached_profile_graph_record,
    graph_duplicate_storage::{
        copy_duplicate_graph_files, ExistingTargetPolicy, GraphPublicationReservation,
    },
    graph_projection_service::hosted_graph_entry_from_record,
    graph_service::{
        read_graph_record, repair_published_graph_projections_best_effort,
        write_graph_record_canonical, GraphRecord,
    },
    ids::{normalize_title, validate_local_id},
    paths::{ensure_graph_layout, graphs_dir, profile_dir},
    profile_service::touch_profile_updated_at,
    rdf::graph_subject,
    rdf_authority::user_rdf_graph_iri,
    rdf_query_service::load_rdf_into_store,
    rdf_service::{dump_rdf, open_graph_store, reconcile_workspace_snapshot, RdfDumpInput},
    runtime_config::{GRAPH_STATUS_ACTIVE, GRAPH_STATUS_DELETED},
    storage::display_path,
};
use oxigraph::{sparql::SparqlEvaluator, store::Store};
use std::path::Path;
#[cfg(test)]
use std::sync::{Arc, Condvar, Mutex};
#[cfg(feature = "desktop")]
use tauri::Manager;
use uuid::Uuid;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DuplicateFailurePoint {
    StageReserved,
    LayoutReady,
    FilesCopied,
    RdfReady,
    ActiveManifestReady,
}

#[cfg(test)]
struct DuplicateTestControl {
    first_lease_acquired: tokio::sync::Notify,
    blocking_checkpoint_reached: tokio::sync::Notify,
    blocking_release: (Mutex<bool>, Condvar),
    pause_at: Option<DuplicateFailurePoint>,
    fail_at: Option<DuplicateFailurePoint>,
    fail_cleanup_remove: bool,
}

#[cfg(test)]
impl DuplicateTestControl {
    fn new() -> Self {
        Self {
            first_lease_acquired: tokio::sync::Notify::new(),
            blocking_checkpoint_reached: tokio::sync::Notify::new(),
            blocking_release: (Mutex::new(false), Condvar::new()),
            pause_at: None,
            fail_at: None,
            fail_cleanup_remove: false,
        }
    }

    fn release_blocking_checkpoint(&self) {
        let (lock, condition) = &self.blocking_release;
        if let Ok(mut released) = lock.lock() {
            *released = true;
            condition.notify_all();
        }
    }
}

pub(crate) async fn duplicate_graph(
    app: AppHandle,
    source_graph_id: String,
    new_graph_id: String,
    new_title: Option<String>,
) -> Result<serde_json::Value, String> {
    #[cfg(test)]
    {
        duplicate_graph_inner(app, source_graph_id, new_graph_id, new_title, None).await
    }
    #[cfg(not(test))]
    {
        duplicate_graph_inner(app, source_graph_id, new_graph_id, new_title).await
    }
}

async fn duplicate_graph_inner(
    app: AppHandle,
    source_graph_id: String,
    new_graph_id: String,
    new_title: Option<String>,
    #[cfg(test)] test_control: Option<Arc<DuplicateTestControl>>,
) -> Result<serde_json::Value, String> {
    validate_local_id(&source_graph_id, "source_graph_id")?;
    validate_local_id(&new_graph_id, "new_graph_id")?;
    if source_graph_id == new_graph_id {
        return Err("source_graph_id and new_graph_id must differ".to_string());
    }

    // The retired frontend-authority build still needs its cooperative flush
    // handshake. Normal desktop and gardend builds both use the leased Rust
    // room flush below as their source snapshot boundary.
    #[cfg(feature = "frontend-crdt")]
    crate::crdt_projection_flush::flush_graph_projection(app.clone(), &source_graph_id).await?;

    let coordinator = app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
        .ok_or_else(|| "graph persistence coordinator is unavailable".to_string())?;
    let (first_graph_id, second_graph_id) = if source_graph_id < new_graph_id {
        (source_graph_id.as_str(), new_graph_id.as_str())
    } else {
        (new_graph_id.as_str(), source_graph_id.as_str())
    };
    let _first_lease = coordinator
        .acquire_lifecycle_exclusive(first_graph_id)
        .await?;
    #[cfg(test)]
    if let Some(control) = test_control.as_ref() {
        control.first_lease_acquired.notify_waiters();
    }
    let _second_lease = coordinator
        .acquire_lifecycle_exclusive(second_graph_id)
        .await?;

    let (source_graph_dir, source_graph_before_flush) = read_graph_record(&app, &source_graph_id)?;
    let title = duplicate_graph_title(&source_graph_before_flush.title, new_title.as_deref())?;

    // The executor normally owns this lease before dispatching crdt.flush.
    // Duplication already owns the source lifecycle lease, so enqueueing would
    // deadlock when the executor tried to reacquire it. Apply the same flush
    // operation directly while source + target authority is held instead.
    flush_source_projection_under_lease(&app, &source_graph_id).await?;
    // The flush may advance content_revision. Build the target manifest from
    // the post-flush record so its RDF seed marker covers the copied snapshot.
    let (flushed_source_dir, source_graph) = read_graph_record(&app, &source_graph_id)?;
    if flushed_source_dir != source_graph_dir {
        return Err("source graph path changed while duplicate authority was held".to_string());
    }

    let target_graph_dir = graphs_dir(&app)?.join(&new_graph_id);
    let target_profile_dir = profile_dir(&app)?;

    let now = timestamp();
    let target_graph = GraphRecord {
        graph_id: new_graph_id.clone(),
        title: title.clone(),
        description: source_graph.description.clone(),
        status: GRAPH_STATUS_ACTIVE.to_string(),
        origin: source_graph.origin.clone(),
        provider_id: source_graph.provider_id.clone(),
        local_path: display_path(&target_graph_dir),
        created_at: now.clone(),
        incarnation_id: Some(Uuid::new_v4().to_string()),
        updated_at: now,
        capabilities: source_graph.capabilities.clone(),
        created_by_operation_id: None,
        validation_policy: source_graph.validation_policy.clone(),
        content_revision: source_graph.content_revision.clone(),
    };

    // Recursive copies and Oxigraph dump/load are synchronous. Keep both
    // ordered lifecycle leases alive while moving that body to Tokio's
    // blocking pool so a large duplicate cannot starve HTTP/WS progress.
    let blocking_app = app.clone();
    let blocking_source_graph_id = source_graph_id.clone();
    let blocking_new_graph_id = new_graph_id.clone();
    let blocking_source_graph_dir = source_graph_dir.clone();
    let blocking_target_graph_dir = target_graph_dir.clone();
    let blocking_target_graph = target_graph.clone();
    let duplicate_result = crate::app_runtime::async_runtime::spawn_blocking(move || {
        duplicate_graph_blocking(
            &blocking_app,
            &blocking_source_graph_id,
            &blocking_new_graph_id,
            &blocking_source_graph_dir,
            &blocking_target_graph_dir,
            &target_profile_dir,
            &blocking_target_graph,
            #[cfg(test)]
            test_control,
        )
    })
    .await
    .map_err(|error| format!("duplicate graph blocking task failed: {error}"))??;
    let (copy_counts, rdf_quad_count) = duplicate_result;

    Ok(serde_json::json!({
        "success": true,
        "type": "duplicate_graph",
        "source_graph_id": source_graph_id.clone(),
        "sourceGraphId": source_graph_id,
        "new_graph_id": new_graph_id.clone(),
        "newGraphId": new_graph_id,
        "title": title,
        "document_count": copy_counts.documents,
        "documentCount": copy_counts.documents,
        "artifact_count": copy_counts.artifacts,
        "artifactCount": copy_counts.artifacts,
        "image_count": copy_counts.images,
        "imageCount": copy_counts.images,
        "s3_objects_copied": copy_counts.documents + copy_counts.artifacts + copy_counts.images,
        "local_objects_copied": copy_counts.documents + copy_counts.artifacts + copy_counts.images,
        "rdf_quad_count": rdf_quad_count,
        "rdfQuadCount": rdf_quad_count,
        "graph": hosted_graph_entry_from_record(target_graph, true),
    }))
}

fn duplicate_graph_blocking(
    app: &AppHandle,
    source_graph_id: &str,
    new_graph_id: &str,
    source_graph_dir: &Path,
    target_graph_dir: &Path,
    target_profile_dir: &Path,
    target_graph: &GraphRecord,
    #[cfg(test)] test_control: Option<Arc<DuplicateTestControl>>,
) -> Result<
    (
        crate::graph_duplicate_storage::DuplicateGraphCopyCounts,
        usize,
    ),
    String,
> {
    // This function contains no awaits: hold one re-entrant durable-plane
    // transaction from hidden staging through atomic publication and its
    // synchronous repair tails. Source flushing and lifecycle lease waits
    // happen before entering this blocking body.
    let _durability_guard = crate::cell_durability::write_guard();
    let publication = GraphPublicationReservation::acquire(
        target_profile_dir,
        target_graph_dir.to_path_buf(),
        new_graph_id,
        ExistingTargetPolicy::Reject,
    )?;
    let stage_result = (|| {
        duplicate_stage_checkpoint(
            #[cfg(test)]
            test_control.as_deref(),
            #[cfg(test)]
            DuplicateFailurePoint::StageReserved,
        )?;
        ensure_graph_layout(publication.stage_dir())?;
        // Staged readers used during RDF rematerialization require a graph
        // manifest. Keep it canonically tombstoned until every essential
        // projection is ready; only the final pre-rename rewrite is active.
        let mut staged_graph = target_graph.clone();
        staged_graph.status = GRAPH_STATUS_DELETED.to_string();
        write_graph_record_canonical(publication.stage_dir(), &staged_graph)
            .map_err(crate::app_error::AppError::message)?;
        duplicate_stage_checkpoint(
            #[cfg(test)]
            test_control.as_deref(),
            #[cfg(test)]
            DuplicateFailurePoint::LayoutReady,
        )?;
        let copy_counts = copy_duplicate_graph_files(
            source_graph_dir,
            publication.stage_dir(),
            target_graph_dir,
            source_graph_id,
            new_graph_id,
        )?;
        duplicate_stage_checkpoint(
            #[cfg(test)]
            test_control.as_deref(),
            #[cfg(test)]
            DuplicateFailurePoint::FilesCopied,
        )?;
        let rdf_quad_count = rematerialize_duplicate_graph_store(
            app.clone(),
            source_graph_id,
            new_graph_id,
            publication.stage_dir(),
            target_graph,
        )?;
        duplicate_stage_checkpoint(
            #[cfg(test)]
            test_control.as_deref(),
            #[cfg(test)]
            DuplicateFailurePoint::RdfReady,
        )?;

        // Oxigraph caches stores by path. Drop the staging-path cache before
        // moving its RocksDB directory, otherwise the first published open can
        // contend with a stale handle for the same files.
        crate::rdf_store_service::evict_graph_store(publication.stage_dir())?;
        write_graph_record_canonical(publication.stage_dir(), target_graph)
            .map_err(crate::app_error::AppError::message)?;
        duplicate_stage_checkpoint(
            #[cfg(test)]
            test_control.as_deref(),
            #[cfg(test)]
            DuplicateFailurePoint::ActiveManifestReady,
        )?;
        publication.publish()?;

        // Publication is now authoritative and cannot be rolled back. These
        // projections are repairable tails and must not turn a visible graph
        // into an operation failure.
        publication.sync_publication_parents_best_effort();
        repair_published_graph_projections_best_effort(app, target_graph);
        Ok((copy_counts, rdf_quad_count))
    })();

    #[cfg(test)]
    let inject_cleanup_failure = test_control
        .as_deref()
        .map(|control| control.fail_cleanup_remove)
        .unwrap_or(false);
    #[cfg(not(test))]
    let inject_cleanup_failure = false;
    match stage_result {
        Ok(result) => Ok(result),
        Err(error) => Err(cleanup_failed_duplicate(
            app,
            new_graph_id,
            &publication,
            error,
            inject_cleanup_failure,
        )),
    }
}

#[cfg(not(test))]
fn duplicate_stage_checkpoint() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
fn duplicate_stage_checkpoint(
    control: Option<&DuplicateTestControl>,
    point: DuplicateFailurePoint,
) -> Result<(), String> {
    let Some(control) = control else {
        return Ok(());
    };
    if control.pause_at == Some(point) {
        control.blocking_checkpoint_reached.notify_waiters();
        let (lock, condition) = &control.blocking_release;
        let mut released = lock
            .lock()
            .map_err(|_| "duplicate blocking checkpoint lock poisoned".to_string())?;
        while !*released {
            let (next, timeout) = condition
                .wait_timeout(released, std::time::Duration::from_secs(30))
                .map_err(|_| "duplicate blocking checkpoint wait poisoned".to_string())?;
            released = next;
            if timeout.timed_out() && !*released {
                return Err("duplicate blocking checkpoint release timed out".to_string());
            }
        }
    }
    if control.fail_at == Some(point) {
        return Err(format!("injected duplicate failure at {point:?}"));
    }
    Ok(())
}

async fn flush_source_projection_under_lease(
    app: &AppHandle,
    source_graph_id: &str,
) -> Result<(), String> {
    let now = timestamp();
    let operation = CrdtOperation {
        operation_id: format!("duplicate-flush-{}", Uuid::new_v4().simple()),
        kind: "crdt.flush".to_string(),
        graph_id: source_graph_id.to_string(),
        document_id: None,
        payload: serde_json::json!({
            "includeMaterialization": true,
            "hydratePersistedSidecars": true,
        }),
        enqueue_timestamp: now,
    };
    crate::crdt_engine::flush_ops::apply(app, &operation)
        .await
        .map(|_| ())
}

fn cleanup_failed_duplicate(
    app: &AppHandle,
    target_graph_id: &str,
    publication: &GraphPublicationReservation,
    primary_error: String,
    inject_remove_failure: bool,
) -> String {
    let mut cleanup_errors = Vec::new();
    if let Some(coordinator) =
        app.try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
    {
        if let Err(error) = coordinator.advance_generation(target_graph_id) {
            cleanup_errors.push(format!("advance target generation: {error}"));
        }
    }
    if let Some(registry) = app.try_state::<crate::crdt_engine::rooms::RoomRegistry>() {
        registry.evict_graph(target_graph_id);
    }
    cleanup_errors.extend(publication.cleanup_failed_stage(inject_remove_failure));
    match profile_dir(app).and_then(|profile_dir| {
        delete_cached_profile_graph_record(&profile_dir, target_graph_id)
            .map_err(|error| error.message())
    }) {
        Ok(()) => {}
        Err(error) => cleanup_errors.push(format!("remove target catalog projection: {error}")),
    }
    if let Err(error) = touch_profile_updated_at(app) {
        cleanup_errors.push(format!("touch profile after duplicate cleanup: {error}"));
    }

    if cleanup_errors.is_empty() {
        primary_error
    } else {
        format!(
            "{primary_error}; additionally failed cleanup: {}",
            cleanup_errors.join("; ")
        )
    }
}

fn rematerialize_duplicate_graph_store(
    app: AppHandle,
    source_graph_id: &str,
    new_graph_id: &str,
    target_graph_dir: &Path,
    target_graph: &GraphRecord,
) -> Result<usize, String> {
    let source_dump = dump_rdf(
        app.clone(),
        RdfDumpInput {
            graph_id: source_graph_id.to_string(),
            format: "n-triples".to_string(),
            source_graph_iri: Some(user_rdf_graph_iri(source_graph_id)),
        },
    )?;
    let store = open_graph_store(target_graph_dir)?;
    if !source_dump.data.trim().is_empty() {
        let rewritten =
            rewrite_duplicate_user_rdf(&source_dump.data, source_graph_id, new_graph_id);
        load_rdf_into_store(
            &store,
            &rewritten,
            "n-triples",
            None,
            Some(&user_rdf_graph_iri(new_graph_id)),
        )?;
    }

    delete_graph_rdf_subject(&store, source_graph_id)?;
    crate::rdf_record_materializer::reconcile_graph_record(&store, target_graph)?;
    if let Some(snapshot) = read_workspace_record(target_graph_dir, new_graph_id)?.snapshot {
        reconcile_workspace_snapshot(&store, new_graph_id, &snapshot)?;
    }
    for document in read_graph_documents_cold(target_graph_dir)? {
        crate::document_meaningful_object::reconcile_document_record(&store, &document)?;
    }
    store
        .len()
        .map_err(|error| format!("count duplicate graph quads: {error}"))
}

fn rewrite_duplicate_user_rdf(
    rdf_n_triples: &str,
    source_graph_id: &str,
    target_graph_id: &str,
) -> String {
    rdf_n_triples
        .replace(
            &format!("<{}", graph_subject(source_graph_id)),
            &format!("<{}", graph_subject(target_graph_id)),
        )
        .replace(
            &format!("graphs/{source_graph_id}/"),
            &format!("graphs/{target_graph_id}/"),
        )
}

fn delete_graph_rdf_subject(store: &Store, graph_id: &str) -> Result<(), String> {
    let subject = graph_subject(graph_id);
    let update = format!(
        r#"
DELETE {{
  <{subject}> ?p ?o .
  ?s ?incoming_p <{subject}> .
}}
WHERE {{
  OPTIONAL {{ <{subject}> ?p ?o . }}
  OPTIONAL {{ ?s ?incoming_p <{subject}> . }}
}}
"#
    );
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse duplicate graph cleanup update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("clean duplicate graph RDF metadata: {error}"))
}

fn duplicate_graph_title(
    source_title: &str,
    requested_title: Option<&str>,
) -> Result<String, String> {
    if let Some(value) = requested_title
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return normalize_title(value);
    }
    let prefix = "Copy of ";
    let max_source_chars = 96usize.saturating_sub(prefix.chars().count());
    let source = source_title
        .chars()
        .take(max_source_chars)
        .collect::<String>();
    normalize_title(&format!("{prefix}{}", source.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_graph_title_uses_request_or_copy_prefix() {
        assert_eq!(
            duplicate_graph_title("Source Graph", Some("Requested")).expect("requested title"),
            "Requested"
        );
        assert_eq!(
            duplicate_graph_title("Source Graph", None).expect("fallback title"),
            "Copy of Source Graph"
        );
    }

    #[test]
    fn duplicate_user_rdf_retargets_only_self_identity_and_storage_paths() {
        let source = concat!(
            "<urn:mnemosyne:local:graph:source-graph:thing> <urn:predicate> ",
            "<urn:mnemosyne:local:graph:source-graph> .\n",
            "<urn:external:subject> <urn:predicate> <urn:mnemosyne:local:graph:external-graph> .\n",
            "<urn:storage> <urn:key> \"users/default/graphs/source-graph/documents/doc-a\" .\n",
            "<urn:opaque> <urn:label> \"source-graph\" .\n",
        );
        let rewritten = rewrite_duplicate_user_rdf(source, "source-graph", "target-graph");
        assert!(rewritten.contains("<urn:mnemosyne:local:graph:target-graph:thing>"));
        assert!(rewritten.contains("<urn:mnemosyne:local:graph:target-graph>"));
        assert!(rewritten.contains("<urn:mnemosyne:local:graph:external-graph>"));
        assert!(rewritten.contains("users/default/graphs/target-graph/documents/doc-a"));
        assert!(rewritten.contains("\"source-graph\""));
        assert!(!rewritten.contains("<urn:mnemosyne:local:graph:source-graph"));
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn concurrent_real_create_list_read_and_connect_observe_one_atomic_publication() {
        use crate::graph_service::{
            create_graph_service, create_graph_service_async, list_graphs_service, CreateGraphInput,
        };
        #[cfg(feature = "desktop")]
        use tauri::Manager;

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-duplicate-create-race-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let source_graph_id = "duplicate-real-create-source";
                let target_graph_id = "duplicate-real-create-target";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Duplicate source".to_string(),
                        graph_id: Some(source_graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create source graph");

                let mut control = DuplicateTestControl::new();
                control.pause_at = Some(DuplicateFailurePoint::FilesCopied);
                let control = Arc::new(control);
                let checkpoint = control.blocking_checkpoint_reached.notified();
                tokio::pin!(checkpoint);
                checkpoint.as_mut().enable();
                let duplicate_app = app.clone();
                let duplicate_control = control.clone();
                let duplicate = crate::app_runtime::async_runtime::spawn(async move {
                    duplicate_graph_inner(
                        duplicate_app,
                        source_graph_id.to_string(),
                        target_graph_id.to_string(),
                        None,
                        Some(duplicate_control),
                    )
                    .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(30), checkpoint)
                    .await
                    .expect("duplicate reached hidden file-copy stage");

                let target_dir = graphs_dir(&app).expect("graphs dir").join(target_graph_id);
                assert!(
                    !target_dir.exists(),
                    "staging leaked into active graph membership"
                );
                assert!(list_graphs_service(&app)
                    .expect("list during duplicate")
                    .iter()
                    .all(|graph| graph.graph_id != target_graph_id));
                assert!(crate::graph_record_store::read_graph_record_no_heal(
                    &app,
                    target_graph_id
                )
                .is_err());

                let create_app = app.clone();
                let (create_started_tx, create_started_rx) = tokio::sync::oneshot::channel();
                let (create_done_tx, mut create_done_rx) = tokio::sync::mpsc::channel(1);
                let create = crate::app_runtime::async_runtime::spawn(async move {
                    let _ = create_started_tx.send(());
                    let result = create_graph_service_async(
                        &create_app,
                        CreateGraphInput {
                            title: "Racing create".to_string(),
                            graph_id: Some(target_graph_id.to_string()),
                            description: None,
                            operation_id: None,
                        },
                    )
                    .await;
                    let _ = create_done_tx.send(result).await;
                });
                create_started_rx.await.expect("real create started");

                let (connected_tx, mut connected_rx) = tokio::sync::mpsc::channel(1);
                let connect_app = app.clone();
                let connect = crate::app_runtime::async_runtime::spawn(async move {
                    let coordinator = connect_app.state::<
                        crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                    >();
                    let _lease = coordinator
                        .acquire_lifecycle_shared(target_graph_id)
                        .await
                        .expect("target connection lease");
                    let result =
                        crate::graph_paths::existing_graph_dir(&connect_app, target_graph_id);
                    let _ = connected_tx.send(result).await;
                });
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(25),
                        connected_rx.recv(),
                    )
                    .await
                    .is_err(),
                    "connection entered while target was staged"
                );
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(25),
                        create_done_rx.recv(),
                    )
                    .await
                    .is_err(),
                    "real create bypassed publication reservation"
                );
                assert!(!target_dir.exists());

                control.release_blocking_checkpoint();
                tokio::time::timeout(std::time::Duration::from_secs(30), duplicate)
                    .await
                    .expect("duplicate completion timeout")
                    .expect("duplicate task")
                    .expect("duplicate wins publication");
                let create_error =
                    tokio::time::timeout(std::time::Duration::from_secs(30), create_done_rx.recv())
                        .await
                        .expect("create completion timeout")
                        .expect("create result")
                        .expect_err("second real publisher must conflict");
                assert_eq!(
                    create_error.kind(),
                    crate::app_error::AppErrorKind::Conflict
                );
                create.await.expect("create task");

                let connected_dir =
                    tokio::time::timeout(std::time::Duration::from_secs(5), connected_rx.recv())
                        .await
                        .expect("target connection released")
                        .expect("target connection result")
                        .expect("connection sees published target");
                assert_eq!(connected_dir, target_dir);
                connect.await.expect("connection task");

                let listed = list_graphs_service(&app).expect("list after publication");
                assert_eq!(
                    listed
                        .iter()
                        .filter(|graph| graph.graph_id == target_graph_id)
                        .count(),
                    1
                );
                let (_, target) = read_graph_record(&app, target_graph_id).expect("read target");
                assert_eq!(target.title, "Copy of Duplicate source");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn direct_document_save_waits_for_duplicate_source_snapshot_lease() {
        use crate::{
            document_service::{create_document, read_document, CreateDocumentInput},
            document_types::SaveDocumentInput,
            graph_service::{create_graph_service, CreateGraphInput},
        };

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-duplicate-save-race-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let source_graph_id = "duplicate-save-source";
                let target_graph_id = "duplicate-save-target";
                let document_id = "lease-document";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Duplicate save source".to_string(),
                        graph_id: Some(source_graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create source graph");
                create_document(
                    app.clone(),
                    CreateDocumentInput {
                        graph_id: source_graph_id.to_string(),
                        title: "Lease document".to_string(),
                        document_id: Some(document_id.to_string()),
                    },
                )
                .expect("create source document");
                crate::document_persistence_service::save_document(
                    app.clone(),
                    serde_json::from_value(serde_json::json!({
                        "graphId": source_graph_id,
                        "documentId": document_id,
                        "title": "Lease document",
                        "body": "before duplicate"
                    }))
                    .expect("seed save input"),
                )
                .expect("seed source document");

                let mut control = DuplicateTestControl::new();
                control.pause_at = Some(DuplicateFailurePoint::FilesCopied);
                let control = Arc::new(control);
                let checkpoint = control.blocking_checkpoint_reached.notified();
                tokio::pin!(checkpoint);
                checkpoint.as_mut().enable();
                let duplicate_app = app.clone();
                let duplicate_control = control.clone();
                let duplicate = crate::app_runtime::async_runtime::spawn(async move {
                    duplicate_graph_inner(
                        duplicate_app,
                        source_graph_id.to_string(),
                        target_graph_id.to_string(),
                        None,
                        Some(duplicate_control),
                    )
                    .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(30), checkpoint)
                    .await
                    .expect("duplicate reached copied-files checkpoint");

                let save_app = app.clone();
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let save_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let save_finished_task = save_finished.clone();
                let save = crate::app_runtime::async_runtime::spawn_blocking(move || {
                    let _ = started_tx.send(());
                    let input: SaveDocumentInput = serde_json::from_value(serde_json::json!({
                        "graphId": source_graph_id,
                        "documentId": document_id,
                        "title": "Lease document",
                        "body": "after duplicate"
                    }))
                    .expect("concurrent save input");
                    let result =
                        crate::document_persistence_service::save_document(save_app, input);
                    save_finished_task.store(true, std::sync::atomic::Ordering::SeqCst);
                    result
                });
                started_rx.await.expect("direct save started");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                assert!(
                    !save_finished.load(std::sync::atomic::Ordering::SeqCst),
                    "direct save bypassed the duplicate's source graph lease"
                );

                control.release_blocking_checkpoint();
                duplicate
                    .await
                    .expect("duplicate task")
                    .expect("duplicate source snapshot");
                save.await
                    .expect("direct save task")
                    .expect("direct save after duplicate");

                let target = read_document(
                    app.clone(),
                    target_graph_id.to_string(),
                    document_id.to_string(),
                )
                .expect("read duplicated document");
                assert_eq!(target.body, "before duplicate");
                let source =
                    read_document(app, source_graph_id.to_string(), document_id.to_string())
                        .expect("read saved source document");
                assert_eq!(source.body, "after duplicate");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn reserved_target_directory_is_not_clobbered_by_create_or_duplicate() {
        use crate::graph_service::{create_graph_service, list_graphs_service, CreateGraphInput};

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-duplicate-reserved-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let source_graph_id = "duplicate-reserved-source";
                let target_graph_id = "duplicate-reserved-target";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Reserved source".to_string(),
                        graph_id: Some(source_graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create reserved source");
                let reserved_dir = graphs_dir(&app).expect("graphs dir").join(target_graph_id);
                std::fs::create_dir_all(&reserved_dir).expect("create reserved target directory");
                let sentinel = reserved_dir.join("reservation-owner.txt");
                std::fs::write(&sentinel, b"must survive").expect("write reservation sentinel");

                let duplicate_error = duplicate_graph_inner(
                    app.clone(),
                    source_graph_id.to_string(),
                    target_graph_id.to_string(),
                    None,
                    None,
                )
                .await
                .expect_err("duplicate must respect reserved directory");
                assert!(
                    duplicate_error.contains("graph already exists"),
                    "{duplicate_error}"
                );
                let create_error = create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Reserved create".to_string(),
                        graph_id: Some(target_graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect_err("create must respect reserved directory");
                assert_eq!(
                    create_error.kind(),
                    crate::app_error::AppErrorKind::Conflict
                );
                assert_eq!(
                    std::fs::read(&sentinel).expect("read sentinel"),
                    b"must survive"
                );
                assert!(!reserved_dir.join("graph.json").exists());
                assert!(list_graphs_service(&app)
                    .expect("list reserved target")
                    .iter()
                    .all(|graph| graph.graph_id != target_graph_id));
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn every_staging_failure_is_invisible_even_when_recursive_cleanup_fails() {
        use crate::graph_service::{create_graph_service, list_graphs_service, CreateGraphInput};

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-duplicate-stage-fail-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let source_graph_id = "duplicate-stage-failure-source";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Failure source".to_string(),
                        graph_id: Some(source_graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create failure source");

                for (index, point) in [
                    DuplicateFailurePoint::StageReserved,
                    DuplicateFailurePoint::LayoutReady,
                    DuplicateFailurePoint::FilesCopied,
                    DuplicateFailurePoint::RdfReady,
                    DuplicateFailurePoint::ActiveManifestReady,
                ]
                .into_iter()
                .enumerate()
                {
                    let target_graph_id = format!("duplicate-stage-failure-{index}");
                    let mut control = DuplicateTestControl::new();
                    control.fail_at = Some(point);
                    control.fail_cleanup_remove =
                        point == DuplicateFailurePoint::ActiveManifestReady;
                    let error = duplicate_graph_inner(
                        app.clone(),
                        source_graph_id.to_string(),
                        target_graph_id.clone(),
                        None,
                        Some(Arc::new(control)),
                    )
                    .await
                    .expect_err("injected stage failure");
                    assert!(error.contains("injected duplicate failure"), "{error}");
                    if point == DuplicateFailurePoint::ActiveManifestReady {
                        assert!(
                            error.contains("injected staged directory removal failure"),
                            "{error}"
                        );
                    }

                    let target_dir = graphs_dir(&app).expect("graphs dir").join(&target_graph_id);
                    assert!(
                        !target_dir.exists(),
                        "failed target became visible at {point:?}"
                    );
                    assert!(crate::graph_record_store::read_graph_record_no_heal(
                        &app,
                        &target_graph_id,
                    )
                    .is_err());
                    assert!(list_graphs_service(&app)
                        .expect("list after failure")
                        .iter()
                        .all(|graph| graph.graph_id != target_graph_id));

                    let staging_root = profile.join(".graph-staging");
                    if staging_root.is_dir() {
                        for entry in std::fs::read_dir(&staging_root).expect("read staging root") {
                            let entry = entry.expect("staging entry");
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if entry.path().is_dir()
                                && name.starts_with(&format!("{target_graph_id}-"))
                            {
                                let manifest_path = entry.path().join("graph.json");
                                if manifest_path.is_file() {
                                    let manifest: GraphRecord =
                                        crate::storage::read_json(&manifest_path)
                                            .expect("read abandoned staged manifest");
                                    assert_eq!(
                                        manifest.status,
                                        crate::runtime_config::GRAPH_STATUS_DELETED,
                                        "cleanup failure retained active canonical manifest"
                                    );
                                }
                            }
                        }
                    }
                }
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn duplicate_hydrates_fresh_registry_hot_only_and_stale_cold_sidecars() {
        use crate::graph_service::{create_graph_service, CreateGraphInput};
        use serde_json::json;
        #[cfg(feature = "desktop")]
        use tauri::Manager;
        use yrs::{Any, Doc, Map, MapPrelim, ReadTxn, StateVector, Transact, WriteTxn};

        fn full_update(doc: &Doc) -> Vec<u8> {
            doc.transact()
                .encode_state_as_update_v1(&StateVector::default())
        }
        fn workspace_update(document_id: &str, title: &str) -> Vec<u8> {
            let doc = Doc::new();
            {
                let mut txn = doc.transact_mut();
                let documents = txn.get_or_insert_map("documents");
                let entry = documents.insert(&mut txn, document_id, MapPrelim::default());
                entry.insert(&mut txn, "title", title);
                entry.insert(&mut txn, "parentId", Any::Null);
                entry.insert(&mut txn, "section", "documents");
                entry.insert(&mut txn, "order", 1.0);
                entry.insert(&mut txn, "createdAt", 1.0);
                entry.insert(&mut txn, "updatedAt", 1.0);
                entry.insert(&mut txn, "readOnly", false);
            }
            full_update(&doc)
        }
        fn document_update(block_id: &str, text: &str) -> Vec<u8> {
            let doc = crate::crdt_engine::builder::ydoc_from_tiptap_json(&json!({
                "type": "doc",
                "content": [{
                    "type": "paragraph",
                    "attrs": { "data-block-id": block_id },
                    "content": [{ "type": "text", "text": text }],
                }],
            }));
            full_update(&doc)
        }

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-duplicate-sidecars-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let source_graph_id = "duplicate-sidecar-source";
                let target_graph_id = "duplicate-sidecar-target";
                let stale_document_id = "stale-cold-document";
                let hot_document_id = "hot-only-document";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Sidecar source".to_string(),
                        graph_id: Some(source_graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create sidecar source");
                let (source_dir, _) = read_graph_record(&app, source_graph_id).expect("source dir");
                let app_registry = app.state::<crate::crdt_engine::rooms::RoomRegistry>();
                let workspace_room = app_registry
                    .get_or_create(
                        &format!("workspace:{source_graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&source_dir),
                    )
                    .await
                    .expect("initial workspace room");
                workspace_room
                    .apply_client_update(&workspace_update(stale_document_id, "Stale Cold"))
                    .await
                    .expect("initial workspace update");
                let stale_room = app_registry
                    .get_or_create(
                        &format!("doc:{source_graph_id}:{stale_document_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&source_dir, stale_document_id),
                    )
                    .await
                    .expect("initial stale document room");
                stale_room
                    .apply_client_update(&document_update("old-block", "old cold sentinel"))
                    .await
                    .expect("initial stale document update");
                flush_source_projection_under_lease(&app, source_graph_id)
                    .await
                    .expect("seed initial cold projection");
                let source_original_dir = source_dir
                    .join("documents")
                    .join(stale_document_id)
                    .join("original");
                std::fs::create_dir_all(&source_original_dir)
                    .expect("create source original directory");
                let source_original_file = source_original_dir.join("source.txt");
                std::fs::write(&source_original_file, b"duplicate original sentinel")
                    .expect("write source original");
                crate::storage::write_json(
                    &source_original_dir.join("manifest.json"),
                    &crate::original_file_types::OriginalFileManifest {
                        source_filename: None,
                        filename: "source.txt".to_string(),
                        mime_type: "text/plain".to_string(),
                        size_bytes: 27,
                        local_path: display_path(&source_original_file),
                        created_at: timestamp(),
                        updated_at: timestamp(),
                    },
                )
                .expect("write source original manifest");
                app_registry.evict_graph(source_graph_id);

                let detached_registry = crate::crdt_engine::rooms::RoomRegistry::default();
                let detached_workspace = detached_registry
                    .get_or_create(
                        &format!("workspace:{source_graph_id}"),
                        crate::ydoc_paths::workspace_ydoc_state_path(&source_dir),
                    )
                    .await
                    .expect("detached workspace room");
                detached_workspace
                    .apply_client_update(&workspace_update(hot_document_id, "Hot Only"))
                    .await
                    .expect("hot-only workspace update");
                let detached_stale = detached_registry
                    .get_or_create(
                        &format!("doc:{source_graph_id}:{stale_document_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&source_dir, stale_document_id),
                    )
                    .await
                    .expect("detached stale room");
                detached_stale
                    .apply_client_update(&document_update(
                        "new-block",
                        "newer hot sidecar sentinel",
                    ))
                    .await
                    .expect("advance stale sidecar");
                let detached_hot = detached_registry
                    .get_or_create(
                        &format!("doc:{source_graph_id}:{hot_document_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&source_dir, hot_document_id),
                    )
                    .await
                    .expect("detached hot-only room");
                detached_hot
                    .apply_client_update(&document_update(
                        "hot-only-block",
                        "hot-only sidecar sentinel",
                    ))
                    .await
                    .expect("write hot-only sidecar");
                drop(detached_registry);

                assert!(app_registry
                    .peek(&format!("workspace:{source_graph_id}"))
                    .await
                    .is_none());
                let stale_before = crate::document_service::read_document(
                    app.clone(),
                    source_graph_id.to_string(),
                    stale_document_id.to_string(),
                )
                .expect("stale cold record before duplicate");
                assert!(!stale_before.body.contains("newer hot sidecar sentinel"));
                assert!(crate::document_service::read_document(
                    app.clone(),
                    source_graph_id.to_string(),
                    hot_document_id.to_string(),
                )
                .is_err());

                let outcome = duplicate_graph_inner(
                    app.clone(),
                    source_graph_id.to_string(),
                    target_graph_id.to_string(),
                    None,
                    None,
                )
                .await
                .expect("duplicate fresh-registry sidecars");
                assert_eq!(outcome["documentCount"], 2);
                let stale_target = crate::document_service::read_document(
                    app.clone(),
                    target_graph_id.to_string(),
                    stale_document_id.to_string(),
                )
                .expect("target stale-cold document");
                assert!(stale_target.body.contains("newer hot sidecar sentinel"));
                assert!(stale_target.local_path.contains(target_graph_id));
                assert!(!stale_target.local_path.contains(".graph-staging"));
                assert!(stale_target.ydoc_state_path.contains(target_graph_id));
                assert!(!stale_target.ydoc_state_path.contains(".graph-staging"));
                let hot_target = crate::document_service::read_document(
                    app.clone(),
                    target_graph_id.to_string(),
                    hot_document_id.to_string(),
                )
                .expect("target hot-only document");
                assert!(hot_target.body.contains("hot-only sidecar sentinel"));

                let (target_dir, _) = read_graph_record(&app, target_graph_id).expect("target dir");
                let target_original_dir = target_dir
                    .join("documents")
                    .join(stale_document_id)
                    .join("original");
                let target_original: crate::original_file_types::OriginalFileManifest =
                    crate::storage::read_json(&target_original_dir.join("manifest.json"))
                        .expect("target original manifest");
                assert_eq!(
                    target_original.local_path,
                    display_path(&target_original_dir.join("source.txt"))
                );
                assert!(!target_original.local_path.contains(".graph-staging"));
                let workspace: serde_json::Value = crate::storage::read_json(
                    &crate::ydoc_paths::workspace_snapshot_path(&target_dir),
                )
                .expect("target workspace projection");
                assert!(workspace["documents"]
                    .as_array()
                    .expect("workspace documents")
                    .iter()
                    .any(|document| document["id"] == hot_document_id));
                let store = open_graph_store(&target_dir).expect("target RDF store");
                for document_id in [stale_document_id, hot_document_id] {
                    let projection = crate::rdf_authority::document_projection_graph_iri(
                        target_graph_id,
                        document_id,
                    );
                    let result = SparqlEvaluator::new()
                        .parse_query(&format!(
                            "ASK WHERE {{ GRAPH <{projection}> {{ ?s ?p ?o }} }}"
                        ))
                        .expect("parse target projection ASK")
                        .on_store(&store)
                        .execute()
                        .expect("query target projection");
                    assert!(matches!(
                        result,
                        oxigraph::sparql::QueryResults::Boolean(true)
                    ));
                }
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(all(feature = "headless", not(feature = "desktop")))]
    #[test]
    fn large_duplicate_blocking_body_allows_current_thread_runtime_progress() {
        use crate::graph_service::{create_graph_service, CreateGraphInput};

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-duplicate-progress-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");
            runtime.block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let source_graph_id = "duplicate-progress-source";
                let target_graph_id = "duplicate-progress-target";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "Progress source".to_string(),
                        graph_id: Some(source_graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create progress source");
                let (source_dir, _) = read_graph_record(&app, source_graph_id).expect("source dir");
                let large_path = source_dir.join("artifacts/large-copy-sentinel.bin");
                std::fs::write(&large_path, vec![0x5a; 8 * 1024 * 1024])
                    .expect("write large copy sentinel");

                let mut control = DuplicateTestControl::new();
                control.pause_at = Some(DuplicateFailurePoint::FilesCopied);
                let control = Arc::new(control);
                let reached = control.blocking_checkpoint_reached.notified();
                tokio::pin!(reached);
                reached.as_mut().enable();
                let duplicate_app = app.clone();
                let duplicate_control = control.clone();
                let duplicate = tokio::spawn(async move {
                    duplicate_graph_inner(
                        duplicate_app,
                        source_graph_id.to_string(),
                        target_graph_id.to_string(),
                        None,
                        Some(duplicate_control),
                    )
                    .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(30), reached)
                    .await
                    .expect("blocking copy checkpoint");

                let (tick_tx, tick_rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    let _ = tick_tx.send(());
                });
                tokio::time::timeout(std::time::Duration::from_secs(1), tick_rx)
                    .await
                    .expect("current-thread runtime stayed schedulable")
                    .expect("progress tick");
                control.release_blocking_checkpoint();
                tokio::time::timeout(std::time::Duration::from_secs(30), duplicate)
                    .await
                    .expect("large duplicate timeout")
                    .expect("large duplicate task")
                    .expect("large duplicate");
                let target_large = graphs_dir(&app)
                    .expect("graphs dir")
                    .join(target_graph_id)
                    .join("artifacts/large-copy-sentinel.bin");
                assert_eq!(
                    std::fs::metadata(target_large)
                        .expect("copied large file")
                        .len(),
                    8 * 1024 * 1024
                );
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
