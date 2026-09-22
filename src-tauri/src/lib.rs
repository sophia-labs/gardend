#![recursion_limit = "512"]

mod active_documents;
mod app_error;
mod app_error_codes;
pub mod app_runtime;
mod artifact_ingest_payloads;
mod artifact_ingest_service;
mod artifact_ingest_title;
mod artifact_kinds;
mod artifact_mcp_service;
mod artifact_revisions;
mod artifact_text_service;
mod agent_status;
mod custom_css;
mod loopback_custom_css_routes;
mod artifact_upload_service;
#[cfg(feature = "headless")]
mod capture_event;
mod cell_durability;
mod cell_durability_trace;
mod cell_graph_boundary;
mod cell_lease;
mod cell_lifecycle;
mod cell_registry_authority;
mod chat_persistence;
pub mod conversation_archive;
mod clock;
pub mod crdt_engine;
mod crdt_operation_audit;
mod crdt_operation_journal;
mod crdt_operation_queue;
mod crdt_operation_types;
mod crdt_projection_flush;
mod crdt_queue;
mod crdt_timing;
mod document_block_projection;
mod document_block_rendering;
mod document_delete_service;
mod document_digest_connections;
mod document_digest_service;
mod document_export_http;
mod document_export_rendering;
mod document_history_file_store;
mod document_history_hosted;
mod document_legacy_history;
mod document_history_mcp;
mod document_history_persistence;
mod document_history_projection;
mod document_history_service;
mod document_history_store;
mod document_hosted_projection;
mod document_hosted_write_payloads;
mod document_incarnation_store;
mod document_mcp_blocks;
mod document_mcp_service;
mod document_mcp_write_payloads;
mod document_create_once_mcp;
// ADDITIVE Meaningful-Object projection path — a parallel, tested alternative to
// the production materializer. `save_document` does NOT call it (it is exercised
// by the parity tests + is the drop-in a future switch would use), so its public
// API reads as dead code in non-test builds; that is by design.
#[allow(dead_code)]
mod document_meaningful_object;
mod pdf_source;
mod pdf_source_wire;
mod document_mutation_service;
mod document_paths;
mod document_persistence_service;
mod document_projection_service;
mod document_record_store;
mod document_rendering;
mod document_service;
mod document_sidecar_store;
mod document_tombstone_store;
mod document_body_availability;
mod document_types;
mod document_ydoc_projection;
mod emporium;
mod emporium_mcp_surface;
mod flow_board;
mod flow_board_mcp;
mod flow_board_reconcile;
mod geist_memory_archive;
mod geist_memory_backfill;
mod geist_memory_rdf;
mod geist_memory_recall_service;
mod geist_memory_service;
mod geist_memory_store;
mod geist_projection_document;
mod geist_service;
mod geist_song_blocks;
mod geist_song_lines;
mod geist_song_projection;
mod geist_song_rdf;
mod geist_song_service;
mod geist_song_store;
mod graph_catalog_projection;
mod graph_catalog_store;
mod graph_duplicate_service;
mod graph_duplicate_storage;
mod graph_intuition_mcp;
mod graph_maintenance_service;
mod graph_metadata_materializer;
mod graph_paths;
mod graph_projection_service;
mod graph_rdf_terms;
mod graph_record_store;
mod graph_service;
mod graph_usage_service;
mod hosted_credentials;
mod hosted_entity_payloads;
mod hosted_entity_read_service;
mod hosted_entity_service;
mod hosted_mode;
mod hosted_navigation_projection;
mod hosted_oauth;
mod hosted_search_projection;
mod hosted_wire_projection;
mod ids;
mod ingestion_approach_static;
mod ingestion_approach_types;
mod ingestion_approaches;
mod json_utils;
mod jsonl_mutation_lock;
mod kg_ultra_intuition_mo;
mod lme_recall_benchmark;
mod local_crdt_jobs;
mod local_job_db;
mod local_job_record_builder;
mod local_job_registry;
#[cfg(test)]
mod local_job_registry_tests;
mod local_job_results;
mod local_job_store;
mod local_job_transitions;
mod local_job_types;
mod local_jobs;
mod local_provider_keys;
mod local_service_host;
mod loopback_ai_routes;
mod loopback_artifact_ingest_routes;
mod loopback_artifact_routes;
mod loopback_audit_log;
mod loopback_batch_routes;
// `pub` (not plain `mod`, unlike every sibling here): `tests/observatory_authority.rs`
// is a separate integration-test crate and needs a real path to the harness
// functions inside. See `observatory/mod.rs` for why this is safe (a
// curated bridge, not a general publicity grant — mirrors `pub mod headless`
// below).
pub mod observatory;
// Depends on `capture_event` (headless-only): normalizes the
// gateway-forwarded `x-sophia-capture-identity` header into cell request
// scope. See plans/observatory-capture-phase1-spine-spec-20260718.md.
#[cfg(feature = "headless")]
mod loopback_capture_identity;
mod loopback_client_token_audit;
mod loopback_client_token_records;
mod loopback_client_token_scope_resolution;
mod loopback_client_token_service;
mod loopback_client_token_store;
mod loopback_client_token_types;
mod loopback_client_tokens;
mod loopback_core_routes;
mod loopback_crdt_routes;
mod loopback_document_history_routes;
mod loopback_document_inputs;
mod loopback_document_routes;
mod loopback_entity_routes;
mod loopback_graph_export_routes;
mod loopback_graph_inputs;
mod loopback_graph_job_routes;
mod loopback_graph_preflight_routes;
mod loopback_graph_routes;
mod loopback_graph_workspace_routes;
mod loopback_hocuspocus_routes;
mod loopback_hosted_document_mutation_routes;
mod loopback_hosted_document_read_routes;
mod loopback_hosted_graph_lifecycle_routes;
mod loopback_hosted_graph_read_routes;
mod loopback_http;
mod loopback_image_routes;
mod loopback_import_job_response;
mod loopback_import_routes;
mod loopback_import_uploads;
mod loopback_ingestion_routes;
mod loopback_job_responses;
mod loopback_knowledge_scopes;
mod loopback_manifest_command;
mod loopback_mcp_routes;
mod loopback_navigation_payloads;
mod loopback_navigation_read_routes;
mod loopback_navigation_routes;
mod loopback_pdf_ingest_routes;
mod loopback_rdf_import_routes;
mod loopback_rdf_routes;
mod loopback_restore_routes;
mod loopback_router;
mod loopback_salience_routes;
mod loopback_scope_catalog;
mod loopback_scope_types;
mod loopback_scopes;
mod loopback_semantic_routes;
mod loopback_server;
mod loopback_service_routes;
mod loopback_state;
mod loopback_system_scopes;
mod loopback_time_travel_routes;
mod loopback_token_grants;
mod loopback_web_import_routes;
mod loopback_wire_routes;
mod loopback_workspace_document_routes;
mod loopback_workspace_scopes;
mod mcp_arg_utils;
mod mcp_block_mutation_service;
mod mcp_block_payloads;
mod mcp_delete_service;
mod mcp_dispatch_registry;
mod mcp_graph_service;
mod mcp_job_service;
mod mcp_rpc_protocol;
mod mcp_search_service;
mod mcp_tool_dispatch;
mod mcp_tool_registry;
mod mcp_utils;
mod mcp_wire_payloads;
mod mcp_wire_read_service;
mod mcp_workspace_comment_payloads;
mod mcp_workspace_crdt_service;
mod mcp_workspace_delete_targets;
mod mcp_workspace_mutation_service;
mod mcp_workspace_payloads;
mod mcp_workspace_raw_payloads;
#[cfg(all(test, feature = "headless"))]
mod meaningful_objects_fixture;
mod memory_semantic_recall_service;
mod model_setup_paths;
mod multipart_pending_upload;
// First-launch onboarding (welcome-graph seed). Desktop-only: the seed command
// is wired through the native invoke-handler and the bundled archive
// (`include_bytes!` of onboarding-v2.tar.gz) has no role in a headless cell,
// which is seeded by the platform rather than a bundled welcome graph.
mod omphalos;
#[cfg(feature = "desktop")]
mod onboarding_archive;
#[cfg(feature = "desktop")]
mod onboarding_service;
mod operation_completion_ledger;
mod orientation_service;
mod original_file_access_tokens;
mod original_file_http;
mod original_file_manifest_store;
mod original_file_service;
mod original_file_storage;
mod original_file_types;
mod paths;
mod pdf_docling_job;
mod pdf_docling_runtime_manifest;
mod pdf_ingest_job_types;
mod pdf_ingest_jobs;
mod pdf_ingest_payloads;
mod pdf_ingest_submission;
mod pdf_ingest_upload;
mod pdf_ingestion_commands;
mod pdf_parsers;
mod pdf_pipeline;
mod pdf_pipeline_catalog;
mod pdf_pipeline_config;
mod pdf_pipeline_descriptors;
mod pdf_pipeline_engine_specs;
mod pdf_pipeline_python_runtime;
mod pdf_pipeline_resolution;
mod pdf_pipeline_runtime;
mod pdf_pymupdf_job;
mod pdf_runtime_processes;
mod pdf_runtimes;
mod pending_upload_paths;
mod pending_upload_service;
mod process_utils;
mod profile_lock;
mod profile_metadata_db;
mod profile_paths;
mod profile_rdf_store_service;
mod profile_service;
mod rdf;
mod rdf_authority;
mod rdf_document_tree;
mod rdf_document_tree_terms;
mod rdf_document_tree_text;
mod rdf_import_service;
mod rdf_mcp_inputs;
mod rdf_query_service;
mod rdf_record_materializer;
mod rdf_seed_service;
mod rdf_service;
mod rdf_store_service;
mod rdf_wire_materializer;
mod rdf_workspace_entity_triples;
mod rdf_workspace_materializer;
mod rdf_workspace_store_materializer;
mod rdf_workspace_terms;
mod rdf_workspace_values;
mod restore_guard;
mod runtime_capabilities;
mod runtime_config;
mod salience_config_projection;
mod salience_important_blocks;
mod salience_mcp_inputs;
mod salience_mcp_valuation;
mod salience_rdf_materializer;
mod salience_route_inputs;
mod salience_route_service;
mod salience_score_projection;
mod salience_score_values;
mod salience_service;
mod salience_value_config;
mod salience_value_scoring;
mod salience_value_store;
mod salience_wire_context;
mod search_lexical_projection;
mod search_projection_service;
mod semantic_embedder;
mod semantic_embedder_backend;
mod semantic_index;
mod semantic_index_paths;
mod semantic_index_progress;
mod semantic_index_refresh;
mod semantic_index_refresh_jobs;
mod semantic_index_status;
mod semantic_index_utils;
mod semantic_mcp_inputs;
mod semantic_model_catalog;
mod semantic_model_prepare_jobs;
mod semantic_model_remote;
mod semantic_model_runtime;
mod semantic_model_state;
mod semantic_model_status;
mod semantic_model_status_types;
mod semantic_models;
mod semantic_reasoner;
mod semantic_relation;
mod semantic_scaffold;
mod semantic_scaffold_rdf;
mod semantic_search_projection;
mod semantic_search_service;
mod semantic_service;
mod source_sync;
mod source_pull_budget;
#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod ludus_source_tests;
mod graph_incarnation_admission;
#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod ludus_incarnation_tests;
mod sparql_admission;
mod storage;
mod storage_atomic;
mod storage_file_ops;
mod tauri_runtime;
mod text_utils;
mod time_travel_interval_scheduler;
mod time_travel_mcp;
mod time_travel_paths;
mod time_travel_restore_service;
mod time_travel_service;
mod time_travel_store;
mod time_travel_types;
mod verbalize;
mod web_fetch;
// Native title-bar appearance (macOS overlay/frameless modes). GUI-only: pulls
// Tauri webview/window APIs (WebviewWindowBuilder, ns_window, objc2), so it is
// excluded from the headless cell build.
#[cfg(feature = "desktop")]
mod window_settings;
mod wire_predicates;
mod wire_projection_service;
mod wire_state;
mod wire_traversal_payloads;
mod wire_traversal_service;
mod workflow_authoring_mcp;
mod workflow_book_mcp;
mod workflow_run_mcp;
mod workspace_document_projection;
mod workspace_entity_projection;
mod workspace_projection_service;
mod workspace_record_store;
mod ydoc_paths;
mod youtube_transcript_format;
mod youtube_transcript_service;
mod youtube_transcript_sources;

use active_documents::set_active_documents;
use chat_persistence::{
    chat_delete_session, chat_load_session, chat_load_sessions, chat_persist_session,
};
use crdt_queue::{complete_crdt_operation, poll_crdt_operations};
use document_service::{
    create_document, delete_document, list_documents, read_document, read_workspace, save_document,
    save_document_ydoc_state, save_workspace, save_workspace_ydoc_state, BlockSnapshot,
};
use graph_service::{create_graph, list_graphs};
use hosted_credentials::{
    clear_hosted_credentials, get_hosted_credentials, store_hosted_credentials,
};
use hosted_mode::{
    consume_hosted_mode_recovery_flag, get_hosted_mode_config, set_hosted_mode_config,
};
use hosted_oauth::{start_hosted_oauth_listener, stop_hosted_oauth_listener};
use local_provider_keys::{
    delete_provider_key, get_provider_keys, provider_key_status, store_provider_key,
};
use loopback_audit_log::list_loopback_audit_events;
use loopback_client_tokens::{
    create_loopback_client_token, list_loopback_client_tokens, revoke_loopback_client_token,
};
use loopback_manifest_command::get_loopback_manifest;
#[cfg(feature = "desktop")]
use onboarding_service::seed_onboarding_graph;
use original_file_service::{
    adopt_pending_original_file, read_original_file, save_original_file,
    save_original_file_manifest,
};
use pdf_ingestion_commands::{
    get_docling_runtime_status, get_pdf_ingestion_pipeline_status, list_ingestion_approaches,
    prepare_docling_runtime, set_pdf_ingestion_pipeline_config,
};
use pending_upload_service::{cleanup_pending_upload, read_pending_upload_file};
use profile_service::get_profile;
use rdf_service::{dump_rdf, load_rdf, load_rdf_dataset, run_sparql_query, run_sparql_update};
use runtime_capabilities::get_capabilities;
use semantic_service::{
    cancel_prepare_semantic_model_job, cancel_semantic_index_refresh_job,
    get_prepare_semantic_model_job, get_semantic_index_refresh_job, get_semantic_index_status,
    get_semantic_model_status, list_semantic_models, prepare_semantic_model,
    prepare_semantic_model_job, refresh_semantic_index, semantic_reason, semantic_search,
    set_semantic_model_config, start_semantic_index_refresh,
};
#[cfg(feature = "desktop")]
use tauri_runtime::setup_native_app;
#[cfg(feature = "desktop")]
use window_settings::{get_window_appearance, set_window_appearance};

#[cfg(not(feature = "desktop"))]
pub fn run() {
    panic!(
        "garden desktop binary requires the `desktop` feature; use the `gardend` bin for headless"
    );
}

/// Entry points for the headless `gardend` cell binary.
#[cfg(not(feature = "desktop"))]
pub mod headless {
    pub use crate::hosted_mode::RuntimeMode;

    /// Construct the native headless application shell. It owns the managed
    /// state registry shared by all cloned handles and has no GUI lifecycle.
    pub fn new_app() -> crate::app_runtime::App {
        crate::app_runtime::App::new()
    }

    /// Tokio facade used by the Gardend process and the core service layer.
    pub use crate::app_runtime::async_runtime;

    /// Strongly consistent minimal-registry read. Gardend calls this before
    /// durable hydrate so an unknown/tombstoned/stale binding cannot touch
    /// storage.
    pub fn preflight_cell_registry() -> Result<(), String> {
        crate::cell_registry_authority::preflight_before_storage().map(|_| ())
    }

    /// Full core setup (profile lock, managed state, loopback server,
    /// CRDT recovery, scheduler) on a native headless handle.
    pub fn setup(
        handle: &crate::app_runtime::AppHandle,
    ) -> Result<RuntimeMode, Box<dyn std::error::Error>> {
        crate::tauri_runtime::setup_core(handle)
    }

    /// Await recovered-work drain and successful loopback API exposure. Core
    /// setup returns before that asynchronous headless bootstrap completes.
    pub async fn wait_until_ready(handle: &crate::app_runtime::AppHandle) -> Result<(), String> {
        crate::tauri_runtime::wait_for_headless_readiness(handle).await
    }

    /// Warm-open this cell's own graph store so the process-wide store cache
    /// is populated BEFORE the durable flusher's first exclusive
    /// lifecycle-gate window. `rdf_store_service::open_graph_store`'s
    /// cache-hit path is deliberately gate-free (see its module doc), so
    /// after this one call every SPARQL/Emporium open resolves without
    /// waiting on `rdf_store_lifecycle_gate()`. Without it, the first
    /// post-hydration query on a fat cell blocks inside `open_graph_store`
    /// for the whole boot-flush plain-file walk while holding its admission
    /// permit and graph persistence lease, starving every later caller.
    /// Resolves the SAME `Arc<Store>` the request path uses (process-wide
    /// cache keyed by path), never a second competing handle; safe to call
    /// repeatedly.
    pub fn warm_open_cell_graph_store(
        handle: &crate::app_runtime::AppHandle,
        graph_id: &str,
    ) -> Result<(), String> {
        let graph_dir = crate::graph_paths::existing_graph_dir(handle, graph_id)?;
        crate::rdf_store_service::open_graph_store(&graph_dir).map(|_| ())
    }

    /// Durable-plane hydrate/flush machinery for the EFS cell pattern.
    /// Active only when `GARDEN_DURABLE_DIR` is set; see `cell_durability`.
    pub mod durability {
        pub use crate::cell_durability::{
            boot_repair, current_write_epoch, flush, flush_forced, flush_forced_detailed,
            flush_forced_detailed_with_trigger, flush_forced_with_trigger, flush_scheduled,
            flush_scheduled_with_trigger, hydrate, hydrate_detailed, set_durable_dirs,
            BootRepairOutcome, DirtyFlushScheduler, FlushAttemptOutcome, FlushOutcome, HydrateMode,
            HydrateOutcome, SchedulerAction,
        };
        pub use crate::cell_durability_trace::FlushTrigger;
    }

    /// Closed, content-free Observatory testimony emitted by a headless cell.
    pub mod testimony {
        pub use crate::capture_event::{
            install_process_writer, ActivityCounts, BootFailureStage, BootMode, CaptureWriter,
            DependencyDetailCode, ErrorCode, FailedFinalFlush, FlushMeasurements, FlushMode,
            SnapshotId, StopReason, SuccessfulFinalFlush,
        };
    }

    /// Cross-process write lease client (U8 bridge lease, spec §3.3). Active
    /// only when both `GARDEN_DURABLE_DIR` and `GARDEN_DURABLE_EPOCH` are
    /// set; see `cell_lease`.
    pub mod lease {
        pub use crate::cell_lease::{
            handle, init, FirstRenew, InitOutcome, LeaseMode, PublishError, PublishPhase,
            PublishRefusalCode, TerminalReason, WriteLease,
        };
    }

    /// Cell-local activity/quiescence authority used by the headless process.
    pub mod lifecycle {
        pub use crate::cell_lifecycle::{
            AdmissionClosed, BackgroundLease, CellActivitySnapshot, CellLifecycle, CellPhase,
            RequestLease, WebSocketLease,
        };

        pub fn tracker(handle: &crate::app_runtime::AppHandle) -> std::sync::Arc<CellLifecycle> {
            #[cfg(feature = "desktop")]
            use tauri::Manager;
            handle
                .state::<std::sync::Arc<CellLifecycle>>()
                .inner()
                .clone()
        }
    }
}

#[cfg(feature = "desktop")]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(setup_native_app)
        .invoke_handler(tauri::generate_handler![
            get_capabilities,
            get_profile,
            get_hosted_mode_config,
            set_hosted_mode_config,
            consume_hosted_mode_recovery_flag,
            get_hosted_credentials,
            store_hosted_credentials,
            clear_hosted_credentials,
            get_provider_keys,
            store_provider_key,
            delete_provider_key,
            provider_key_status,
            chat_persist_session,
            chat_load_sessions,
            chat_load_session,
            chat_delete_session,
            start_hosted_oauth_listener,
            stop_hosted_oauth_listener,
            get_loopback_manifest,
            list_loopback_audit_events,
            list_loopback_client_tokens,
            create_loopback_client_token,
            revoke_loopback_client_token,
            list_graphs,
            create_graph,
            seed_onboarding_graph,
            operation_completion_ledger::get_completion_entry,
            operation_completion_ledger::append_completion_entry_command,
            list_documents,
            create_document,
            read_document,
            delete_document,
            read_workspace,
            save_workspace,
            save_workspace_ydoc_state,
            save_document,
            save_document_ydoc_state,
            set_active_documents,
            save_original_file,
            save_original_file_manifest,
            adopt_pending_original_file,
            read_original_file,
            read_pending_upload_file,
            cleanup_pending_upload,
            run_sparql_query,
            run_sparql_update,
            load_rdf,
            load_rdf_dataset,
            dump_rdf,
            get_semantic_index_status,
            get_semantic_model_status,
            list_semantic_models,
            set_semantic_model_config,
            get_docling_runtime_status,
            get_pdf_ingestion_pipeline_status,
            set_pdf_ingestion_pipeline_config,
            list_ingestion_approaches,
            prepare_semantic_model,
            prepare_semantic_model_job,
            get_prepare_semantic_model_job,
            cancel_prepare_semantic_model_job,
            prepare_docling_runtime,
            start_semantic_index_refresh,
            get_semantic_index_refresh_job,
            cancel_semantic_index_refresh_job,
            refresh_semantic_index,
            semantic_search,
            semantic_reason,
            poll_crdt_operations,
            complete_crdt_operation,
            get_window_appearance,
            set_window_appearance,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
