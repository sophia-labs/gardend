use crate::{
    clock::timestamp,
    local_crdt_jobs::{insert_and_spawn_crdt_job, LocalCrdtJobInput},
    loopback_http::{loopback_error, require_loopback_scope},
    loopback_import_job_response::import_job_accepted_response,
    loopback_import_uploads::{
        read_archive_import_upload, read_cell_archive_restore_upload, read_graph_import_upload,
    },
    loopback_state::LoopbackState,
    paths::existing_graph_dir,
    profile_paths::profile_dir,
    storage::display_path,
};
use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::sync::Arc;

use crate::cell_graph_boundary::{current_cell_lease, CellRole};

fn import_source_default_folder_name(source_type: &str) -> String {
    let label = match source_type {
        "obsidian" => "Obsidian",
        "notion" => "Notion",
        "roam" => "Roam",
        _ => "Archive",
    };
    format!("Imported {label} {}", timestamp())
}

pub(super) async fn loopback_hosted_import_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.import") {
        return response;
    }

    let profile_root = match profile_dir(&state.app) {
        Ok(profile_dir) => profile_dir,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };

    let upload = match read_graph_import_upload(&profile_root, multipart).await {
        Ok(upload) => upload,
        Err(response) => return response,
    };

    let job_graph_id = upload.new_graph_id.clone();
    let payload = serde_json::json!({
        "filename": upload.filename,
        "newGraphId": upload.new_graph_id,
        "newTitle": upload.new_title,
        "pendingArchivePath": display_path(&upload.pending_archive_path),
        "sizeBytes": upload.size_bytes,
    });
    let record = match insert_and_spawn_crdt_job(
        state,
        LocalCrdtJobInput {
            job_type: "import_graph".to_string(),
            graph_id: job_graph_id,
            operation_kind: "graph.importArchive".to_string(),
            document_id: None,
            detail: serde_json::json!({
                "filename": payload.get("filename").cloned().unwrap_or(serde_json::Value::Null),
                "newGraphId": payload.get("newGraphId").cloned().unwrap_or(serde_json::Value::Null),
                "newTitle": payload.get("newTitle").cloned().unwrap_or(serde_json::Value::Null),
                "sizeBytes": payload.get("sizeBytes").cloned().unwrap_or(serde_json::Value::Null),
                "asyncWork": true,
            }),
            payload,
            pending_cleanup_path: Some(upload.pending_archive_path),
            running_message: "Importing graph archive".to_string(),
            success_message: "Graph archive import complete".to_string(),
            result_mapper: None,
        },
    ) {
        Ok(record) => record,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    import_job_accepted_response(&record)
}

pub(super) async fn loopback_hosted_restore_cell_archive(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.restore") {
        return response;
    }
    let Some(lease) = current_cell_lease() else {
        return loopback_error(
            StatusCode::FORBIDDEN,
            "cell archive restore is available only through an authenticated single-graph cell",
        );
    };
    if lease.role != CellRole::Owner {
        return loopback_error(
            StatusCode::FORBIDDEN,
            "cell archive restore requires the graph owner role",
        );
    }
    if state.cell_graph.owner_graph_id() != Some(graph_id.as_str()) {
        return loopback_error(StatusCode::NOT_FOUND, "graph not found in this cell");
    }
    let Some(target_generation) = state.cell_graph.graph_generation() else {
        return loopback_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "cell archive restore requires a bound graph generation",
        );
    };
    let graph_dir = match existing_graph_dir(&state.app, &graph_id) {
        Ok(graph_dir) => graph_dir,
        Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
    };
    let upload = match read_cell_archive_restore_upload(&graph_dir, multipart).await {
        Ok(upload) => upload,
        Err(response) => return response,
    };
    if upload.target_generation != target_generation {
        crate::paths::cleanup_pending_upload_file(&upload.pending_archive_path);
        return loopback_error(
            StatusCode::CONFLICT,
            "cell archive restore target generation does not match this cell",
        );
    }
    let owner_subject = state
        .cell_graph
        .owner_principal()
        .and_then(|principal| principal.strip_prefix("user:"));
    if owner_subject != Some(upload.source_user_id.as_str()) {
        crate::paths::cleanup_pending_upload_file(&upload.pending_archive_path);
        return loopback_error(
            StatusCode::FORBIDDEN,
            "cell archive source subject does not match the graph owner",
        );
    }
    if upload.source_graph_id != graph_id {
        crate::paths::cleanup_pending_upload_file(&upload.pending_archive_path);
        return loopback_error(
            StatusCode::CONFLICT,
            "cell archive source graph does not match the target graph",
        );
    }

    let mut payload = serde_json::json!({
        "operationId": upload.operation_id,
        "newGraphId": graph_id,
        "filename": upload.filename,
        "pendingArchivePath": display_path(&upload.pending_archive_path),
        "archiveSha256": upload.archive_sha256,
        "sourceGraphId": upload.source_graph_id,
        "sourceUserId": upload.source_user_id,
        "targetGeneration": upload.target_generation,
        "planDigest": upload.plan_digest,
        "expectedDocumentCount": upload.expected_document_count,
        "expectedRdfTripleCount": upload.expected_rdf_triple_count,
        "includesArtifacts": upload.format_version == 2,
        "sizeBytes": upload.size_bytes,
    });
    // Absence keeps the historical v1 completion-envelope bytes unchanged.
    if upload.format_version == 2 {
        payload["formatVersion"] = serde_json::json!(2);
    }
    // Absent for the same reason: an import declaring no concessions must produce the
    // envelope it always did. Strict is the absence of a value, not a value.
    if let Some(parity) = upload.content_parity.clone() {
        payload["contentParity"] = parity;
    }
    let record = match insert_and_spawn_crdt_job(
        state,
        LocalCrdtJobInput {
            job_type: "restore_cell_archive".to_string(),
            graph_id,
            operation_kind: "graph.restoreArchive".to_string(),
            document_id: None,
            detail: serde_json::json!({
                "archiveSha256": payload["archiveSha256"],
                "planDigest": payload["planDigest"],
                "targetGeneration": payload["targetGeneration"],
                "sizeBytes": payload["sizeBytes"],
                "asyncWork": true,
            }),
            payload,
            pending_cleanup_path: Some(upload.pending_archive_path),
            running_message: "Restoring graph archive into empty cell".to_string(),
            success_message: "Cell graph archive restore complete".to_string(),
            result_mapper: None,
        },
    ) {
        Ok(record) => record,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    import_job_accepted_response(&record)
}

async fn loopback_hosted_import_archive(
    state: Arc<LoopbackState>,
    headers: HeaderMap,
    graph_id: String,
    source_type: &'static str,
    multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.import") {
        return response;
    }
    let graph_dir = match existing_graph_dir(&state.app, &graph_id) {
        Ok(graph_dir) => graph_dir,
        Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
    };

    let upload = match read_archive_import_upload(source_type, &graph_dir, multipart).await {
        Ok(upload) => upload,
        Err(response) => return response,
    };
    let effective_folder = upload
        .folder_name
        .unwrap_or_else(|| import_source_default_folder_name(source_type));
    let payload = serde_json::json!({
        "sourceType": source_type,
        "filename": upload.filename,
        "folderName": effective_folder,
        "pendingArchivePath": display_path(&upload.pending_archive_path),
        "sizeBytes": upload.size_bytes,
    });
    let record = match insert_and_spawn_crdt_job(
        state,
        LocalCrdtJobInput {
            job_type: format!("import_{source_type}"),
            graph_id,
            operation_kind: "import.vault".to_string(),
            document_id: None,
            detail: serde_json::json!({
                "sourceType": source_type,
                "filename": payload.get("filename").cloned().unwrap_or(serde_json::Value::Null),
                "folderName": payload.get("folderName").cloned().unwrap_or(serde_json::Value::Null),
                "sizeBytes": payload.get("sizeBytes").cloned().unwrap_or(serde_json::Value::Null),
                "asyncWork": true,
            }),
            payload,
            pending_cleanup_path: Some(upload.pending_archive_path),
            running_message: format!("Importing {source_type} archive"),
            success_message: format!("Imported {source_type} archive"),
            result_mapper: None,
        },
    ) {
        Ok(record) => record,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    import_job_accepted_response(&record)
}

pub(super) async fn loopback_hosted_import_obsidian(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    loopback_hosted_import_archive(state, headers, graph_id, "obsidian", multipart).await
}

pub(super) async fn loopback_hosted_import_notion(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    loopback_hosted_import_archive(state, headers, graph_id, "notion", multipart).await
}

pub(super) async fn loopback_hosted_import_roam(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    loopback_hosted_import_archive(state, headers, graph_id, "roam", multipart).await
}
