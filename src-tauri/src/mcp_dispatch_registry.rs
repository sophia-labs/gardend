// Runtime dispatch registry for MCP `tools/call`. The MCP envelope's
// `tools/list` registry lives in `mcp_tool_registry.rs` (catalog + scope
// metadata); this module is the parallel surface that maps a tool name
// straight to its handler so the `tools/call` dispatcher does not carry
// an implementation table.
//
// Every entry is `(name, handler)`. Handlers are sync or async functions
// that return `AppResult<Value>`. Trampolines (`tramp_*`) bridge the
// existing handler shapes — `(AppHandle, &Value)`, `(&LocalJobRegistry,
// &Value)`, `(AppHandle, Arc<LocalJobRegistry>, &Value)`, etc. — into the
// uniform `Fn(&McpCallCtx, &Value) -> AppResult<Value>` shape without
// touching the handler bodies. Legacy `Result<Value, String>` handlers
// are funneled through `AppError::internal` at the trampoline so the
// dispatcher boundary needs only a single `String::from` shim.
//
// Adding a tool: write the handler, add a trampoline below, append an
// `entry!` row to `REGISTRY`, and add the catalog/scope rows in their
// respective sources of truth.

use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    artifact_ingest_service::{mcp_local_ingest_artifact, mcp_local_upload_artifact},
    artifact_mcp_service::{
        mcp_local_edit_artifact_image, mcp_local_list_artifact_kinds,
        mcp_local_list_artifact_revisions, mcp_local_read_artifact,
        mcp_local_restore_artifact_revision,
    },
    document_digest_service::mcp_local_document_digest,
    document_history_mcp::{mcp_local_get_document_history, mcp_local_read_document_at_snapshot},
    document_mcp_blocks::{mcp_local_get_block, mcp_local_query_blocks, mcp_local_read_blocks},
    document_mcp_service::{mcp_local_list_documents, mcp_local_read_document},
    document_mutation_service::mcp_local_write_document,
    emporium::{
        chamber::propose_domain_ontology,
        mcp::{mcp_local_emporium_violations, mcp_local_emporium_vocab},
    },
    emporium_mcp_surface::{
        mcp_local_emporium_heads, mcp_local_emporium_list, mcp_local_emporium_query,
        mcp_local_emporium_read, mcp_local_emporium_retract, mcp_local_emporium_sweep,
        mcp_local_emporium_write, mcp_local_sparql_query_named,
    },
    geist_memory_backfill::mcp_local_backfill_memory_projection,
    geist_memory_recall_service::mcp_local_recall_memories,
    geist_memory_service::{
        mcp_local_archive_memories, mcp_local_care_memories, mcp_local_remember,
        mcp_local_remember_batch,
    },
    geist_service::{mcp_local_context_bundle, mcp_local_quick_orient},
    geist_song_service::{mcp_local_music, mcp_local_sing},
    graph_catalog_projection::mcp_local_list_graphs,
    graph_intuition_mcp::mcp_local_graph_intuition,
    local_jobs::LocalJobRegistry,
    mcp_block_mutation_service::{
        mcp_local_delete_blocks, mcp_local_edit_block_text, mcp_local_insert_blocks,
        mcp_local_update_blocks,
    },
    mcp_delete_service::mcp_local_delete,
    mcp_graph_service::{
        mcp_local_create_graph, mcp_local_duplicate_graph, mcp_local_manage_graph,
        mcp_local_query_graph, mcp_local_reindex_graph, mcp_local_update_graph,
    },
    mcp_job_service::{mcp_local_cancel_job, mcp_local_get_job_result, mcp_local_get_job_status},
    mcp_search_service::{mcp_local_search_blocks, mcp_local_search_documents},
    mcp_wire_read_service::{mcp_local_get_wires, mcp_local_list_wire_predicates},
    mcp_workspace_crdt_service::{
        mcp_local_crdt_operation, mcp_local_create_document, mcp_local_create_folder,
        mcp_local_delete_document, mcp_local_flush_crdt, mcp_local_make_document_editable,
        mcp_local_move_documents, mcp_local_move_folder,
    },
    mcp_workspace_mutation_service::{
        mcp_local_create_wires, mcp_local_edit_comment, mcp_local_rename,
    },
    memory_semantic_recall_service::mcp_local_memory_semantic_recall,
    orientation_service::mcp_local_surface,
    rdf_service::{
        mcp_local_rdf_dump, mcp_local_rdf_load, mcp_local_sparql_query, mcp_local_sparql_update,
    },
    salience_important_blocks::mcp_local_get_important_blocks,
    salience_service::{mcp_local_get_block_values, mcp_local_get_values, mcp_local_revaluate},
    semantic_reasoner::mcp_local_semantic_reason,
    semantic_search_service::mcp_local_semantic_search,
    source_sync::{
        mcp_authoritative_value, mcp_local_source_pull, mcp_local_source_push,
        mcp_local_source_rebuild,
    },
    time_travel_mcp::{
        mcp_local_cancel_restore_operation, mcp_local_create_restore_point,
        mcp_local_delete_restore_point, mcp_local_diff_restore_points,
        mcp_local_get_restore_operation, mcp_local_get_restore_point,
        mcp_local_list_restore_points, mcp_local_restore_to_restore_point,
    },
    wire_traversal_service::mcp_local_traverse_wires,
    workflow_authoring_mcp::mcp_local_workflow_authoring_session,
    workflow_book_mcp::{
        mcp_local_workflow_book_apply, mcp_local_workflow_book_choose,
        mcp_local_workflow_book_compose, mcp_local_workflow_book_open,
        mcp_local_workflow_book_validate,
    },
    workflow_run_mcp::{mcp_local_workflow_run_monitor, mcp_local_workflow_run_start},
    workspace_projection_service::mcp_local_get_workspace,
};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};

pub(crate) struct McpCallCtx<'a> {
    pub(crate) app: AppHandle,
    pub(crate) jobs: &'a Arc<LocalJobRegistry>,
}

pub(crate) type McpHandlerFut<'a> = Pin<Box<dyn Future<Output = AppResult<Value>> + Send + 'a>>;
pub(crate) type McpHandler = for<'a> fn(&'a McpCallCtx<'a>, &'a Value) -> McpHandlerFut<'a>;

pub(crate) struct McpToolEntry {
    pub(crate) name: &'static str,
    pub(crate) handler: McpHandler,
}

pub(crate) fn lookup(name: &str) -> Option<&'static McpToolEntry> {
    REGISTRY.iter().find(|entry| entry.name == name)
}

// Trampolines bridge each handler's call shape onto the uniform
// `Fn(&McpCallCtx, &Value) -> AppResult<Value>` shape. Bodies stay
// untouched. Legacy `Result<Value, String>` handlers are mapped through
// `AppError::internal` here so the dispatcher boundary only needs a
// single `String::from` shim.

fn tramp_list_graphs<'a>(ctx: &'a McpCallCtx<'a>, _args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_graphs(app).map_err(AppError::internal) })
}

fn tramp_quick_orient<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_quick_orient(app, args).map_err(AppError::internal) })
}

fn tramp_context_bundle<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_context_bundle(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_remember<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_remember(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_remember_batch<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_remember_batch(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_recall<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_recall_memories(app, args).map_err(AppError::internal) })
}

fn tramp_propose_domain_ontology<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { propose_domain_ontology(&app, args).await })
}

fn tramp_care<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_care_memories(app, args).map_err(AppError::internal) })
}

fn tramp_emporium_vocab<'a>(_ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    Box::pin(async move { mcp_local_emporium_vocab(args) })
}

fn tramp_emporium_list<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_list(app, args) })
}

fn tramp_emporium_read<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_read(app, args) })
}

fn tramp_emporium_heads<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_heads(app, args) })
}

fn tramp_emporium_sweep<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_sweep(app, args) })
}

fn tramp_sparql_query_named<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_sparql_query_named(app, args) })
}

fn tramp_emporium_query<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_query(app, args) })
}

fn tramp_emporium_write<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_write(app, args).await })
}

fn tramp_emporium_retract<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_retract(app, args).await })
}

fn tramp_emporium_violations<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_violations(app, args) })
}

fn tramp_backfill_memory_projection<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_backfill_memory_projection(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_archive_memories<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_archive_memories(app, args).map_err(AppError::internal) })
}

fn tramp_music<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_music(app, args).map_err(AppError::internal) })
}

fn tramp_sing<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_sing(app, args).map_err(AppError::internal) })
}

fn tramp_surface<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_surface(app, args).map_err(AppError::internal) })
}

fn tramp_get_document_history<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_document_history(app, args).map_err(AppError::internal) })
}

fn tramp_read_document_at_snapshot<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(
        async move { mcp_local_read_document_at_snapshot(app, args).map_err(AppError::internal) },
    )
}

fn tramp_create_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_create_graph(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_duplicate_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_duplicate_graph(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_manage_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_manage_graph(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_update_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_update_graph(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_query_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_query_graph(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_get_job_status<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(
        async move { mcp_local_get_job_status(jobs.as_ref(), args).map_err(AppError::internal) },
    )
}

fn tramp_get_job_result<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(
        async move { mcp_local_get_job_result(jobs.as_ref(), args).map_err(AppError::internal) },
    )
}

fn tramp_cancel_job<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_cancel_job(jobs.as_ref(), args).map_err(AppError::internal) })
}

fn tramp_reindex_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_reindex_graph(app, jobs, args).map_err(AppError::internal) })
}

fn tramp_list_documents<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_documents(app, args).map_err(AppError::internal) })
}

fn tramp_get_workspace<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_get_workspace(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_read_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_read_document(app, args).map_err(AppError::internal) })
}

fn tramp_write_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_write_document(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_create_document_once<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        crate::document_create_once_mcp::create_document_once(app, args)
            .await.map_err(AppError::internal)
    })
}

fn tramp_read_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_read_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_document_digest<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_document_digest(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_value<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_authoritative_value(app, args).await })
}

fn tramp_get_values<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_values(app, args).map_err(AppError::internal) })
}

fn tramp_revaluate<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_revaluate(app, args).map_err(AppError::internal) })
}

fn tramp_get_block_values<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_block_values(app, args).map_err(AppError::internal) })
}

fn tramp_get_important_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_important_blocks(app, args).map_err(AppError::internal) })
}

fn tramp_get_block<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_get_block(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_query_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_query_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_search_documents<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_search_documents(app, args).map_err(AppError::internal) })
}

fn tramp_search_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_search_blocks(app, args).map_err(AppError::internal) })
}

fn tramp_list_wire_predicates<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_wire_predicates(app, args).map_err(AppError::internal) })
}

fn tramp_get_wires<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_wires(app, args).map_err(AppError::internal) })
}

fn tramp_create_wires<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_create_wires(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_traverse_wires<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_traverse_wires(app, args).map_err(AppError::internal) })
}

fn tramp_move_folder<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_move_folder(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_rename<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_rename(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_delete<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_delete(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_make_document_editable<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_make_document_editable(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_edit_comment<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_edit_comment(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_upload_artifact<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move {
        mcp_local_upload_artifact(app, jobs, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_ingest_artifact<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move {
        mcp_local_ingest_artifact(app, jobs, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_read_artifact<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_read_artifact(app, args).await })
}
fn tramp_create_artifact_text<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    Box::pin(crate::artifact_text_service::mcp_mutate(ctx.app.clone(),args,"create"))
}
fn tramp_read_custom_css<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    Box::pin(crate::custom_css::mcp_read(ctx.app.clone(), args))
}
fn tramp_agent_status<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    Box::pin(crate::agent_status::read(ctx.app.clone(), args))
}
fn tramp_status<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    Box::pin(crate::agent_status::write(ctx.app.clone(), args))
}
fn tramp_custom_css<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    Box::pin(crate::custom_css::mcp_capability(ctx.app.clone(), args))
}
fn tramp_write_custom_css<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    Box::pin(crate::custom_css::mcp_write(ctx.app.clone(), args))
}
fn tramp_write_artifact_text<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    Box::pin(crate::artifact_text_service::mcp_mutate(ctx.app.clone(),args,"write"))
}

fn tramp_list_artifact_kinds<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_artifact_kinds(app, args) })
}

fn tramp_list_artifact_revisions<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_artifact_revisions(app, args) })
}

fn tramp_restore_artifact_revision<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_restore_artifact_revision(app, args) })
}

fn tramp_edit_artifact_image<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_edit_artifact_image(app, args).await })
}

fn tramp_create_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_create_document(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_delete_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_delete_document(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_create_folder<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_create_folder(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_move_documents<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_move_documents(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_flush_crdt<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_flush_crdt(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_crdt_operation<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_crdt_operation(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_insert_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_insert_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_update_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_update_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_edit_block_text<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_edit_block_text(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_delete_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_delete_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_sparql_query<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_sparql_query(app, args).await })
}

fn tramp_sparql_update<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_sparql_update(app, args).await })
}

fn tramp_rdf_load<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_rdf_load(app, args) })
}

fn tramp_rdf_dump<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_rdf_dump(app, args) })
}

fn tramp_workflow_book_open<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_workflow_book_open(app, args) })
}

fn tramp_workflow_book_choose<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_workflow_book_choose(app, args) })
}

fn tramp_workflow_book_apply<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_workflow_book_apply(app, args).await })
}

fn tramp_workflow_book_compose<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_workflow_book_compose(app, args).await })
}

fn tramp_workflow_book_validate<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_workflow_book_validate(app, args) })
}

fn tramp_workflow_authoring_session<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_workflow_authoring_session(app, jobs, args).await })
}

fn tramp_workflow_run_start<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_workflow_run_start(app, args).await })
}

fn tramp_workflow_run_monitor<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_workflow_run_monitor(app, args).await })
}

fn tramp_semantic_search<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_semantic_search(app, args).map_err(AppError::internal) })
}

fn tramp_memory_semantic_recall<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_memory_semantic_recall(app, args).map_err(AppError::internal) })
}

fn tramp_semantic_reason<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_semantic_reason(app, args).map_err(AppError::internal) })
}

fn tramp_graph_intuition<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_graph_intuition(app, args).await })
}

fn tramp_list_restore_points<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_restore_points(app, args) })
}

fn tramp_get_restore_point<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_restore_point(app, args) })
}

fn tramp_create_restore_point<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_create_restore_point(app, args) })
}

fn tramp_diff_restore_points<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_diff_restore_points(app, args) })
}

fn tramp_delete_restore_point<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_delete_restore_point(app, args) })
}

fn tramp_restore_to_restore_point<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_restore_to_restore_point(app, jobs, args) })
}

fn tramp_get_restore_operation<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_get_restore_operation(jobs.as_ref(), args) })
}

fn tramp_cancel_restore_operation<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_cancel_restore_operation(jobs.as_ref(), args) })
}

fn tramp_source_pull<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_source_pull(app, args).await })
}

fn tramp_source_push<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_source_push(app, args).await })
}

fn tramp_source_rebuild<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_source_rebuild(app, args).await })
}

const REGISTRY: &[McpToolEntry] = &[
    McpToolEntry {
        name: "list_graphs",
        handler: tramp_list_graphs,
    },
    McpToolEntry {
        name: "quick_orient",
        handler: tramp_quick_orient,
    },
    McpToolEntry {
        name: "context_bundle",
        handler: tramp_context_bundle,
    },
    McpToolEntry {
        name: "remember",
        handler: tramp_remember,
    },
    McpToolEntry {
        name: "remember_batch",
        handler: tramp_remember_batch,
    },
    McpToolEntry {
        name: "recall",
        handler: tramp_recall,
    },
    McpToolEntry {
        name: "propose_domain_ontology",
        handler: tramp_propose_domain_ontology,
    },
    McpToolEntry {
        name: "care",
        handler: tramp_care,
    },
    McpToolEntry {
        name: "archive_memories",
        handler: tramp_archive_memories,
    },
    McpToolEntry {
        name: "backfill_memory_projection",
        handler: tramp_backfill_memory_projection,
    },
    McpToolEntry {
        name: "emporium_vocab",
        handler: tramp_emporium_vocab,
    },
    McpToolEntry {
        name: "emporium_list",
        handler: tramp_emporium_list,
    },
    McpToolEntry {
        name: "emporium_read",
        handler: tramp_emporium_read,
    },
    McpToolEntry {
        name: "emporium_heads",
        handler: tramp_emporium_heads,
    },
    McpToolEntry {
        name: "emporium_sweep",
        handler: tramp_emporium_sweep,
    },
    McpToolEntry {
        name: "emporium_query",
        handler: tramp_emporium_query,
    },
    McpToolEntry {
        name: "emporium_write",
        handler: tramp_emporium_write,
    },
    McpToolEntry {
        name: "emporium_retract",
        handler: tramp_emporium_retract,
    },
    McpToolEntry {
        name: "emporium_violations",
        handler: tramp_emporium_violations,
    },
    McpToolEntry {
        name: "music",
        handler: tramp_music,
    },
    McpToolEntry {
        name: "sing",
        handler: tramp_sing,
    },
    McpToolEntry {
        name: "surface",
        handler: tramp_surface,
    },
    McpToolEntry {
        name: "get_document_history",
        handler: tramp_get_document_history,
    },
    McpToolEntry {
        name: "read_document_at_snapshot",
        handler: tramp_read_document_at_snapshot,
    },
    McpToolEntry {
        name: "create_graph",
        handler: tramp_create_graph,
    },
    McpToolEntry {
        name: "duplicate_graph",
        handler: tramp_duplicate_graph,
    },
    McpToolEntry {
        name: "manage_graph",
        handler: tramp_manage_graph,
    },
    McpToolEntry {
        name: "update_graph",
        handler: tramp_update_graph,
    },
    McpToolEntry {
        name: "query_graph",
        handler: tramp_query_graph,
    },
    McpToolEntry {
        name: "get_job_status",
        handler: tramp_get_job_status,
    },
    McpToolEntry {
        name: "get_job_result",
        handler: tramp_get_job_result,
    },
    McpToolEntry {
        name: "cancel_job",
        handler: tramp_cancel_job,
    },
    McpToolEntry {
        name: "reindex_graph",
        handler: tramp_reindex_graph,
    },
    McpToolEntry {
        name: "list_documents",
        handler: tramp_list_documents,
    },
    McpToolEntry {
        name: "get_workspace",
        handler: tramp_get_workspace,
    },
    McpToolEntry {
        name: "read_document",
        handler: tramp_read_document,
    },
    McpToolEntry {
        name: "write_document",
        handler: tramp_write_document,
    },
    McpToolEntry {
        name: "create_document_once",
        handler: tramp_create_document_once,
    },
    McpToolEntry {
        name: "read_blocks",
        handler: tramp_read_blocks,
    },
    McpToolEntry {
        name: "document_digest",
        handler: tramp_document_digest,
    },
    McpToolEntry {
        name: "value",
        handler: tramp_value,
    },
    McpToolEntry {
        name: "get_values",
        handler: tramp_get_values,
    },
    McpToolEntry {
        name: "revaluate",
        handler: tramp_revaluate,
    },
    McpToolEntry {
        name: "get_block_values",
        handler: tramp_get_block_values,
    },
    McpToolEntry {
        name: "get_important_blocks",
        handler: tramp_get_important_blocks,
    },
    McpToolEntry {
        name: "get_block",
        handler: tramp_get_block,
    },
    McpToolEntry {
        name: "query_blocks",
        handler: tramp_query_blocks,
    },
    McpToolEntry {
        name: "search_documents",
        handler: tramp_search_documents,
    },
    McpToolEntry {
        name: "search_blocks",
        handler: tramp_search_blocks,
    },
    McpToolEntry {
        name: "list_wire_predicates",
        handler: tramp_list_wire_predicates,
    },
    McpToolEntry {
        name: "get_wires",
        handler: tramp_get_wires,
    },
    McpToolEntry {
        name: "create_wires",
        handler: tramp_create_wires,
    },
    McpToolEntry {
        name: "traverse_wires",
        handler: tramp_traverse_wires,
    },
    McpToolEntry {
        name: "move_folder",
        handler: tramp_move_folder,
    },
    McpToolEntry {
        name: "rename",
        handler: tramp_rename,
    },
    McpToolEntry {
        name: "delete",
        handler: tramp_delete,
    },
    McpToolEntry {
        name: "make_document_editable",
        handler: tramp_make_document_editable,
    },
    McpToolEntry {
        name: "edit_comment",
        handler: tramp_edit_comment,
    },
    McpToolEntry {
        name: "upload_artifact",
        handler: tramp_upload_artifact,
    },
    McpToolEntry {
        name: "ingest_artifact",
        handler: tramp_ingest_artifact,
    },
    McpToolEntry {
        name: "read_artifact",
        handler: tramp_read_artifact,
    },
    McpToolEntry { name:"agent_status", handler:tramp_agent_status },
    McpToolEntry { name:"status", handler:tramp_status },
    McpToolEntry { name:"read_custom_css", handler:tramp_read_custom_css },
    McpToolEntry { name:"custom_css", handler:tramp_custom_css },
    McpToolEntry { name:"write_custom_css", handler:tramp_write_custom_css },
    McpToolEntry { name:"create_artifact_text", handler:tramp_create_artifact_text },
    McpToolEntry { name:"write_artifact_text", handler:tramp_write_artifact_text },
    McpToolEntry {
        name: "list_artifact_kinds",
        handler: tramp_list_artifact_kinds,
    },
    McpToolEntry {
        name: "list_artifact_revisions",
        handler: tramp_list_artifact_revisions,
    },
    McpToolEntry {
        name: "restore_artifact_revision",
        handler: tramp_restore_artifact_revision,
    },
    McpToolEntry {
        name: "edit_artifact_image",
        handler: tramp_edit_artifact_image,
    },
    McpToolEntry {
        name: "create_document",
        handler: tramp_create_document,
    },
    McpToolEntry {
        name: "delete_document",
        handler: tramp_delete_document,
    },
    McpToolEntry {
        name: "create_folder",
        handler: tramp_create_folder,
    },
    McpToolEntry {
        name: "move_documents",
        handler: tramp_move_documents,
    },
    McpToolEntry {
        name: "flush_crdt",
        handler: tramp_flush_crdt,
    },
    McpToolEntry {
        name: "crdt_operation",
        handler: tramp_crdt_operation,
    },
    McpToolEntry {
        name: "insert_blocks",
        handler: tramp_insert_blocks,
    },
    McpToolEntry {
        name: "update_blocks",
        handler: tramp_update_blocks,
    },
    McpToolEntry {
        name: "edit_block_text",
        handler: tramp_edit_block_text,
    },
    McpToolEntry {
        name: "delete_blocks",
        handler: tramp_delete_blocks,
    },
    McpToolEntry {
        name: "sparql_query",
        handler: tramp_sparql_query,
    },
    McpToolEntry {
        name: "sparql_query_named",
        handler: tramp_sparql_query_named,
    },
    McpToolEntry {
        name: "sparql_update",
        handler: tramp_sparql_update,
    },
    McpToolEntry {
        name: "rdf_load",
        handler: tramp_rdf_load,
    },
    McpToolEntry {
        name: "rdf_dump",
        handler: tramp_rdf_dump,
    },
    McpToolEntry {
        name: "workflow_book_open",
        handler: tramp_workflow_book_open,
    },
    McpToolEntry {
        name: "workflow_book_choose",
        handler: tramp_workflow_book_choose,
    },
    McpToolEntry {
        name: "workflow_book_apply",
        handler: tramp_workflow_book_apply,
    },
    McpToolEntry {
        name: "workflow_book_compose",
        handler: tramp_workflow_book_compose,
    },
    McpToolEntry {
        name: "workflow_book_validate",
        handler: tramp_workflow_book_validate,
    },
    McpToolEntry {
        name: "workflow_authoring_session",
        handler: tramp_workflow_authoring_session,
    },
    McpToolEntry {
        name: "workflow_run_start",
        handler: tramp_workflow_run_start,
    },
    McpToolEntry {
        name: "workflow_run_monitor",
        handler: tramp_workflow_run_monitor,
    },
    McpToolEntry {
        name: "semantic_search",
        handler: tramp_semantic_search,
    },
    McpToolEntry {
        name: "memory_semantic_recall",
        handler: tramp_memory_semantic_recall,
    },
    McpToolEntry {
        name: "semantic_reason",
        handler: tramp_semantic_reason,
    },
    McpToolEntry {
        name: "graph_intuition",
        handler: tramp_graph_intuition,
    },
    McpToolEntry {
        name: "list_restore_points",
        handler: tramp_list_restore_points,
    },
    McpToolEntry {
        name: "get_restore_point",
        handler: tramp_get_restore_point,
    },
    McpToolEntry {
        name: "create_restore_point",
        handler: tramp_create_restore_point,
    },
    McpToolEntry {
        name: "diff_restore_points",
        handler: tramp_diff_restore_points,
    },
    McpToolEntry {
        name: "delete_restore_point",
        handler: tramp_delete_restore_point,
    },
    McpToolEntry {
        name: "restore_to_restore_point",
        handler: tramp_restore_to_restore_point,
    },
    McpToolEntry {
        name: "get_restore_operation",
        handler: tramp_get_restore_operation,
    },
    McpToolEntry {
        name: "cancel_restore_operation",
        handler: tramp_cancel_restore_operation,
    },
    McpToolEntry {
        name: "source_pull",
        handler: tramp_source_pull,
    },
    McpToolEntry {
        name: "source_push",
        handler: tramp_source_push,
    },
    McpToolEntry {
        name: "source_rebuild",
        handler: tramp_source_rebuild,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // Snapshot guard: every catalog tool name must have a dispatch entry,
    // and the registry must not carry tools that are not in the catalog.
    // This is the structural insurance against drift between
    // `mcp_tool_catalog.json` and the runtime dispatch table.
    #[test]
    fn registry_covers_every_catalog_tool_exactly() {
        #[derive(serde::Deserialize)]
        struct Catalog {
            tools: Vec<CatalogTool>,
        }
        #[derive(serde::Deserialize)]
        struct CatalogTool {
            name: String,
        }
        let catalog: Catalog = serde_json::from_str(include_str!("mcp_tool_catalog.json"))
            .expect("embedded MCP tool catalog must be valid JSON");

        let registry_names: BTreeSet<&'static str> =
            REGISTRY.iter().map(|entry| entry.name).collect();
        let catalog_names: BTreeSet<String> =
            catalog.tools.iter().map(|tool| tool.name.clone()).collect();

        assert_eq!(
            REGISTRY.len(),
            registry_names.len(),
            "registry contains duplicate tool names"
        );
        assert_eq!(
            REGISTRY.len(),
            catalog.tools.len(),
            "registry size ({}) does not match catalog size ({})",
            REGISTRY.len(),
            catalog.tools.len()
        );
        for name in &catalog_names {
            assert!(
                registry_names.contains(name.as_str()),
                "catalog tool {name} has no dispatch entry"
            );
        }
        for name in &registry_names {
            assert!(
                catalog_names.contains(*name),
                "dispatch entry {name} is not in the catalog"
            );
        }
    }

    #[test]
    fn registry_lookup_returns_known_entries() {
        assert!(lookup("list_graphs").is_some());
        assert!(lookup("write_document").is_some());
        assert!(lookup("rdf_load").is_some());
        assert!(lookup("graph_intuition").is_some());
        assert!(lookup("restore_to_restore_point").is_some());
        assert!(lookup("memory_semantic_recall").is_some());
        assert!(lookup("not_a_real_tool").is_none());
    }

    // T0 items 1-3: the emporium read family is now dispatched (not just
    // written) — each name must resolve to a registry entry.
    #[test]
    fn emporium_mcp_family_is_reachable_through_dispatch() {
        assert!(lookup("emporium_vocab").is_some());
        assert!(lookup("emporium_list").is_some());
        assert!(lookup("emporium_read").is_some());
        assert!(lookup("emporium_heads").is_some());
        assert!(lookup("emporium_sweep").is_some());
        assert!(lookup("emporium_violations").is_some());
        assert!(lookup("sparql_query_named").is_some());
        assert!(lookup("emporium_query").is_some());
        assert!(lookup("emporium_write").is_some());
        assert!(lookup("emporium_retract").is_some());
    }
}

// The emporium MCP family needs a REAL per-graph store (`emporium_list`/
// `emporium_read`/`emporium_heads`/`emporium_sweep`/`emporium_violations` all
// resolve one), so the "it actually SERVES, not just resolves" proof runs
// under the headless harness. NO MOCKS: real cell, real store, real spine —
// dispatched through the EXACT `McpCallCtx` + `(entry.handler)(...)` path
// `mcp_tool_dispatch.rs` uses for a live `tools/call`.
#[cfg(all(test, feature = "headless"))]
mod headless_dispatch_tests {
    use super::*;
    use crate::emporium::planner::memory_record_subject;
    use crate::emporium::schemas::{MemoryRecordIn, SourceRefIn};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::local_jobs::LocalJobRegistry;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn env_serial() -> &'static Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-emporium-mcp-dispatch-{name}-{nanos}"))
    }

    fn memory_record(
        client_ref: &str,
        content: &str,
        supersedes: Option<String>,
    ) -> MemoryRecordIn {
        MemoryRecordIn {
            client_ref: Some(client_ref.to_string()),
            scope: "agent".to_string(),
            kind: "ClaimMemory".to_string(),
            content_orientation: "knowledge".to_string(),
            visibility: "private".to_string(),
            status: "active".to_string(),
            content: content.to_string(),
            source_refs: vec![SourceRefIn {
                source_kind: "DocumentBlock".to_string(),
                source_label: None,
                block_id: Some("abc".to_string()),
                document_id: Some("doc-shell".to_string()),
                external_id: None,
                external_uri: None,
                observed_at: None,
                trust_tier: None,
            }],
            evidence: vec![],
            observed_at: Some(1_718_700_000_000),
            valid_from: Some(1_718_700_000_000),
            is_current: Some(true),
            confidence: None,
            valence: None,
            agent_id: Some("gamma".to_string()),
            observer_agent_id: None,
            tags: vec![],
            supersedes_ref: supersedes,
            contradicts_ref: None,
        }
    }

    fn call(ctx: &McpCallCtx<'_>, name: &str, args: Value) -> AppResult<Value> {
        let entry = lookup(name).unwrap_or_else(|| panic!("dispatch entry for {name}"));
        crate::app_runtime::async_runtime::block_on((entry.handler)(ctx, &args))
    }

    #[test]
    fn emporium_mcp_family_is_dispatched_and_serves_real_reads() {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_dir("profile");
        let jobs_dir = temp_dir("jobs");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| run_emporium_family_serve_trace(&jobs_dir));
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        let _ = std::fs::remove_dir_all(&jobs_dir);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn run_emporium_family_serve_trace(jobs_dir: &Path) {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph_id = "emporium-mcp-serve-lab";
        create_graph_service(
            &app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Emporium MCP Serve Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");

        let jobs =
            Arc::new(LocalJobRegistry::new(jobs_dir.to_path_buf()).expect("create job registry"));
        let ctx = McpCallCtx {
            app: app.clone(),
            jobs: &jobs,
        };

        // emporium_vocab: dispatched, serves the registry catalog.
        let vocab =
            call(&ctx, "emporium_vocab", serde_json::json!({})).expect("emporium_vocab serves");
        assert!(vocab["vocabularies"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()));

        // Seed one object through the REAL object surface directly (the
        // object-surface WRITE path is deliberately deferred this wave — only
        // the dispatch of the two READS is under test here).
        crate::app_runtime::async_runtime::block_on(crate::emporium::objects::create_objects(
            &app,
            graph_id,
            "emporium-bookmark",
            serde_json::json!([{
                "kind": "Bookmark",
                "localId": "dispatch-book",
                "url": "https://example.test/book",
                "title": "Dispatch Book",
            }]),
        ))
        .expect("seed bookmark");

        // emporium_list / emporium_read: dispatched, round-trip the seeded object.
        let listed = call(
            &ctx,
            "emporium_list",
            serde_json::json!({ "graph_id": graph_id, "vocab": "emporium-bookmark", "class": "Bookmark" }),
        )
        .expect("emporium_list serves");
        assert_eq!(
            listed["subjects"].as_array().map(Vec::len),
            Some(1),
            "{listed}"
        );

        let read = call(
            &ctx,
            "emporium_read",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "emporium-bookmark", "class": "Bookmark",
                "address": "dispatch-book",
            }),
        )
        .expect("emporium_read serves");
        assert!(
            read["subject"]
                .as_str()
                .is_some_and(|s| s.ends_with("dispatch-book")),
            "{read}"
        );

        // File H, then race two supersessions of H — a genuine contested
        // lineage (both isCurrent=true), the real fixture `emporium_heads` /
        // `emporium_sweep` / `emporium_violations` are built to surface.
        let h = memory_record("r-h", "vera prefers fish CLI", None);
        let h_subject = memory_record_subject(graph_id, &h);
        crate::app_runtime::async_runtime::block_on(crate::geist_memory_service::ingest_memory_record(
            &app, graph_id, h,
        ))
        .expect("file H");
        let a = memory_record("r-a", "vera prefers zsh", Some(h_subject.clone()));
        let b = memory_record("r-b", "vera prefers nushell", Some(h_subject));
        let (ra, rb) = crate::app_runtime::async_runtime::block_on(async {
            tokio::join!(
                crate::geist_memory_service::ingest_memory_record(&app, graph_id, a),
                crate::geist_memory_service::ingest_memory_record(&app, graph_id, b),
            )
        });
        assert_eq!(ra.expect("A ingest ran")["ok"], serde_json::json!(true));
        assert_eq!(rb.expect("B ingest ran")["ok"], serde_json::json!(true));

        // emporium_heads: dispatched, surfaces the contested lineage.
        let heads = call(
            &ctx,
            "emporium_heads",
            serde_json::json!({ "graph_id": graph_id }),
        )
        .expect("emporium_heads serves");
        let lineages = heads["lineages"].as_array().expect("lineages array");
        assert!(
            lineages
                .iter()
                .any(|l| l["contested"] == serde_json::json!(true)),
            "{heads}"
        );

        // emporium_sweep: dispatched, files the finding to the ledger.
        let swept = call(
            &ctx,
            "emporium_sweep",
            serde_json::json!({ "graph_id": graph_id }),
        )
        .expect("emporium_sweep serves");
        assert_eq!(swept["ledgered"], serde_json::json!(1), "{swept}");

        // emporium_violations: dispatched, reads the ledger's own finding back.
        let violations = call(
            &ctx,
            "emporium_violations",
            serde_json::json!({ "graph_id": graph_id }),
        )
        .expect("emporium_violations serves");
        let rows = violations["violations"]
            .as_array()
            .expect("violations array");
        assert!(!rows.is_empty(), "{violations}");
        assert!(
            rows.iter()
                .any(|r| r["severity"] == serde_json::json!("Warning")),
            "{violations}"
        );

        // sparql_query_named: dispatched, currentHeads surfaces the SAME
        // contested lineage `emporium_heads` just proved — ONE semantics,
        // reached through TWO surfaces.
        let named_heads = call(
            &ctx,
            "sparql_query_named",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "sophia-memory-core", "class": "MemoryRecord",
                "query_name": "currentHeads",
            }),
        )
        .expect("sparql_query_named currentHeads serves");
        let named_rows = named_heads["rows"].as_array().expect("rows array");
        assert!(
            named_rows
                .iter()
                .any(|r| r["contested"] == serde_json::json!(true)),
            "{named_heads}"
        );

        // sparql_query_named byId: dispatched, reads the seeded bookmark's span
        // (a non-membrane-ed, SimpleProjection-routed class).
        let named_by_id = call(
            &ctx,
            "sparql_query_named",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "emporium-bookmark", "class": "Bookmark",
                "query_name": "byId", "params": {"subject": read["subject"]},
            }),
        )
        .expect("sparql_query_named byId serves");
        assert!(
            !named_by_id["rows"]
                .as_array()
                .expect("rows array")
                .is_empty(),
            "{named_by_id}"
        );

        // emporium_query (T2 item 11a): dispatched, "objects out" over the
        // SAME contested lineage — hydrated objects, not rows, flagged
        // contested with sibling head refs. Full criteria/observer/
        // perspectives/face/pagination coverage lives in
        // `emporium::object_query`'s own headless suite; this is the
        // "reachable through dispatch" proof.
        let queried = call(
            &ctx,
            "emporium_query",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "sophia-memory-core", "class": "MemoryRecord",
                "criteria": {},
            }),
        )
        .expect("emporium_query serves");
        let queried_objects = queried["objects"].as_array().expect("objects array");
        assert!(
            queried_objects
                .iter()
                .any(|o| o["contested"] == serde_json::json!(true)),
            "{queried}"
        );

        // emporium_query face="markdown": dispatched through the SAME MCP
        // boundary, proving the face switch (not just the direct engine
        // call the object_query suite exercises).
        let queried_markdown = call(
            &ctx,
            "emporium_query",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "sophia-memory-core", "class": "MemoryRecord",
                "criteria": {}, "face": "markdown",
            }),
        )
        .expect("emporium_query face=markdown serves");
        assert_eq!(queried_markdown["face"], serde_json::json!("markdown"));
        let markdown = queried_markdown["markdown"]
            .as_str()
            .expect("markdown string");
        assert!(markdown.contains("MemoryRecord"), "{markdown}");

        // emporium_query face="markdwon" (a typo): a LOUD structured
        // rejection, never a silent fall-through to the JSON default —
        // the catalog declares an enum of legal `face` values, so a typo
        // must not be silently accepted as if it meant "json".
        let bad_face = call(
            &ctx,
            "emporium_query",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "sophia-memory-core", "class": "MemoryRecord",
                "criteria": {}, "face": "markdwon",
            }),
        )
        .unwrap_err();
        assert_eq!(
            bad_face.kind(),
            crate::app_error::AppErrorKind::Validation,
            "{bad_face:?}"
        );
    }
}
