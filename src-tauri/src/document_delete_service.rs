use crate::app_runtime::AppHandle;
use crate::{
    graph_service::touch_graph_content_revision,
    ids::validate_local_id,
    paths::{document_ydoc_dir, documents_dir, existing_graph_dir},
    rdf::{document_subject, sparql_string_literal},
    rdf_authority::document_projection_graph_iri,
    rdf_service::{open_graph_store, MutationResult},
    runtime_config::MNEMO_NS,
    semantic_service::remove_document_from_semantic_index,
    storage::remove_dir_all,
};
use oxigraph::sparql::SparqlEvaluator;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
#[cfg(feature = "desktop")]
use tauri::Manager;

#[cfg(test)]
static FAIL_NEXT_DELETE_AFTER_TOMBSTONE: OnceLock<Mutex<Option<(String, String)>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_document_delete_after_tombstone_for_test(
    graph_id: impl Into<String>,
    document_id: impl Into<String>,
) {
    *FAIL_NEXT_DELETE_AFTER_TOMBSTONE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some((graph_id.into(), document_id.into()));
}

#[cfg(test)]
fn fail_delete_after_tombstone_if_requested(
    graph_id: &str,
    document_id: &str,
) -> Result<(), String> {
    let mut failure = FAIL_NEXT_DELETE_AFTER_TOMBSTONE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failure
        .as_ref()
        .is_some_and(|(expected_graph, expected_document)| {
            expected_graph == graph_id && expected_document == document_id
        })
    {
        *failure = None;
        return Err(format!(
            "injected document directory cleanup failure after tombstone: {graph_id}/{document_id}"
        ));
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_delete_after_tombstone_if_requested(
    _graph_id: &str,
    _document_id: &str,
) -> Result<(), String> {
    Ok(())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(super) fn delete_document(
    app: AppHandle,
    graph_id: String,
    document_id: String,
) -> Result<MutationResult, String> {
    let _lease = crate::crdt_engine::persistence_coordinator::
        acquire_lifecycle_exclusive_blocking_if_managed(&app, &graph_id)?;
    delete_document_for_operation(app, graph_id, document_id, None)
}

pub(crate) fn delete_document_for_operation(
    app: AppHandle,
    graph_id: String,
    document_id: String,
    deletion_operation_id: Option<&str>,
) -> Result<MutationResult, String> {
    // Queue handlers call this only while holding the graph lease. The public
    // Tauri wrapper above acquires the blocking form before entering here.
    // Directory removal, semantic-index cleanup, RDF deletion, and the graph
    // revision form one synchronous recoverable persistence boundary. Do not
    // let the durable-plane walker capture the directory half-removed.
    let _durability_guard = crate::cell_durability::write_guard();
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    validate_local_id(&document_id, "document_id")?;
    crate::document_body_availability::require_available(&graph_dir, &document_id)?;
    crate::document_history_service::with_document_history_lock(&graph_dir, &document_id, || {
        delete_document_locked(
            &app,
            &graph_dir,
            &graph_id,
            &document_id,
            DeleteBoundary::Establish(deletion_operation_id),
        )
    })
}

enum DeleteBoundary<'a> {
    Establish(Option<&'a str>),
    Preserve(&'a str),
}

fn delete_document_locked(
    app: &AppHandle,
    graph_dir: &std::path::Path,
    graph_id: &str,
    document_id: &str,
    boundary: DeleteBoundary<'_>,
) -> Result<MutationResult, String> {
    // This marker is the deletion commit point. Everything below is a
    // repairable tail, and all room/record hydration must fail closed while it
    // exists even if directory removal or a later RDF/index step fails.
    match boundary {
        DeleteBoundary::Establish(operation_id) => {
            crate::document_tombstone_store::write_document_tombstone_for_operation(
                graph_dir,
                document_id,
                operation_id,
            )?;
        }
        DeleteBoundary::Preserve(expected_deletion_id) => {
            let current =
                crate::document_tombstone_store::read_document_tombstone(graph_dir, document_id)?
                    .ok_or_else(|| {
                    format!("document tombstone missing during delete repair: {document_id}")
                })?;
            if current.deletion_id != expected_deletion_id {
                return Err(format!(
                    "document deletion boundary changed during delete repair: {document_id}"
                ));
            }
        }
    }
    if let Some(registry) = app.try_state::<crate::crdt_engine::rooms::RoomRegistry>() {
        registry.evict_room(&format!("doc:{graph_id}:{document_id}"));
    }
    fail_delete_after_tombstone_if_requested(graph_id, document_id)?;

    let document_dir = documents_dir(graph_dir).join(document_id);
    if document_dir.is_dir() {
        remove_dir_all(&document_dir)
            .map_err(|error| format!("delete document directory: {error}"))?;
    }

    let ydoc_dir = document_ydoc_dir(graph_dir, document_id);
    if ydoc_dir.is_dir() {
        remove_dir_all(&ydoc_dir).map_err(|error| format!("delete document ydoc: {error}"))?;
    }
    remove_document_from_semantic_index(graph_dir, document_id)?;

    let subject = document_subject(document_id);
    let authority_graph = document_projection_graph_iri(graph_id, document_id);
    let update = format!(
        r#"
PREFIX mnemo: <{MNEMO_NS}>
DELETE {{
  GRAPH <{authority_graph}> {{ ?projection_s ?projection_p ?projection_o . }}
  <{subject}> ?old_doc_p ?old_doc_o .
  ?tree_subject ?tree_p ?tree_o .
}}
WHERE {{
  OPTIONAL {{
    GRAPH <{authority_graph}> {{ ?projection_s ?projection_p ?projection_o . }}
  }}
  OPTIONAL {{ <{subject}> ?old_doc_p ?old_doc_o . }}
  OPTIONAL {{
    ?tree_subject mnemo:documentId {document_id} .
    ?tree_subject ?tree_p ?tree_o .
  }}
}}
"#,
        document_id = sparql_string_literal(document_id),
    );
    let store = open_graph_store(graph_dir)?;
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse document delete update: {error}"))?
        .on_store(&store)
        .execute()
        .map_err(|error| format!("delete document RDF: {error}"))?;
    crate::pdf_source::remove_document(&store, graph_id, document_id)?;
    touch_graph_content_revision(graph_dir)?;
    let quad_count = store
        .len()
        .map_err(|error| format!("count quads: {error}"))?;
    Ok(MutationResult {
        ok: true,
        quad_count,
    })
}

/// Finish any interrupted delete tail while preserving its tombstone, then
/// return the exact deletion boundary a trusted same-ID recreation must clear
/// only after its fresh document and workspace authorities are durable.
pub(crate) fn prepare_tombstoned_document_recreation(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Option<crate::document_tombstone_store::DocumentTombstone>, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let Some(tombstone) =
        crate::document_tombstone_store::read_document_tombstone(&graph_dir, document_id)?
    else {
        return Ok(None);
    };
    crate::document_history_service::with_document_history_lock(&graph_dir, document_id, || {
        delete_document_locked(
            app,
            &graph_dir,
            graph_id,
            document_id,
            DeleteBoundary::Preserve(&tombstone.deletion_id),
        )
    })?;
    let current =
        crate::document_tombstone_store::read_document_tombstone(&graph_dir, document_id)?
            .ok_or_else(|| {
                format!("document tombstone disappeared during recreation: {document_id}")
            })?;
    if current.deletion_id != tombstone.deletion_id {
        return Err(format!(
            "document deletion boundary changed during recreation: {document_id}"
        ));
    }
    Ok(Some(current))
}
