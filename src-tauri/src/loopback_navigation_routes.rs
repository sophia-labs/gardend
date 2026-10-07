use crate::{
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    hosted_navigation_projection::hosted_navigation_parts,
    json_utils::json_string,
    loopback_http::{loopback_error, require_loopback_scope, require_loopback_scopes},
    loopback_navigation_payloads::{
        apply_artifact_original_manifest, normalize_navigation_artifact_payload,
        normalize_navigation_folder_payload,
    },
    loopback_navigation_read_routes::{
        loopback_hosted_navigation, loopback_hosted_navigation_artifact,
        loopback_hosted_navigation_artifacts, loopback_hosted_navigation_folder,
        loopback_hosted_navigation_folders, loopback_hosted_navigation_job_result,
    },
    loopback_files_routes::{loopback_files_patch, loopback_files_trash},
    loopback_state::LoopbackState,
    original_file_service::save_artifact_original_file,
};
use axum::{
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::Arc;

pub(super) fn loopback_navigation_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/navigation/{graph_id}", get(loopback_hosted_navigation))
        .route(
            "/navigation/{graph_id}/job-result/{job_id}",
            get(loopback_hosted_navigation_job_result),
        )
        .route(
            "/navigation/{graph_id}/folders",
            get(loopback_hosted_navigation_folders),
        )
        .route(
            "/navigation/{graph_id}/folders/{folder_id}",
            get(loopback_hosted_navigation_folder)
                .put(loopback_hosted_put_navigation_folder)
                .delete(loopback_hosted_delete_navigation_folder),
        )
        .route(
            "/navigation/{graph_id}/artifacts",
            get(loopback_hosted_navigation_artifacts),
        )
        .route(
            "/navigation/{graph_id}/artifacts/{artifact_id}",
            get(loopback_hosted_navigation_artifact)
                .put(loopback_hosted_put_navigation_artifact)
                .patch(loopback_files_patch)
                .delete(loopback_hosted_delete_navigation_artifact)
                // A PUT may carry one file as base64: bound the body to the
                // encoded Files cap instead of the 516 MiB router default.
                .layer(DefaultBodyLimit::max(
                    crate::files_service::base64_route_body_limit(),
                )),
        )
}

pub(super) async fn loopback_hosted_put_navigation_folder(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, folder_id)): AxumPath<(String, String)>,
    Json(input): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.write.crdt") {
        return response;
    }
    let input = normalize_navigation_folder_payload(input, &folder_id);
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.createFolder".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(folder_id.clone()),
            payload: input,
        },
    )
    .await
    {
        Ok(_) => {
            if let Err(error) = flush_graph_projection(state.app.clone(), &graph_id).await {
                return loopback_error(StatusCode::BAD_REQUEST, &error);
            }
            match hosted_navigation_parts(&state.app, &graph_id).and_then(|(folders, _, _)| {
                folders
                    .into_iter()
                    .find(|folder| {
                        json_string(folder.get("id")).as_deref() == Some(folder_id.as_str())
                    })
                    .ok_or_else(|| format!("folder {folder_id} not found after update"))
            }) {
                Ok(folder) => Json(folder).into_response(),
                Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
            }
        }
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_delete_navigation_folder(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, folder_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "workspace.delete.crdt") {
        return response;
    }
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.deleteFolder".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(folder_id.clone()),
            payload: serde_json::json!({ "folderId": folder_id.clone() }),
        },
    )
    .await
    {
        Ok(outcome) => {
            if let Err(error) = flush_graph_projection(state.app.clone(), &graph_id).await {
                return loopback_error(StatusCode::BAD_REQUEST, &error);
            }
            Json(outcome.value).into_response()
        }
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_put_navigation_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
    Json(input): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scopes(
        &headers,
        &state,
        &["workspace.write.crdt", "artifacts.write"],
    ) {
        return response;
    }
    let (mut input, original_upload) = normalize_navigation_artifact_payload(input, &artifact_id);
    if let Some(original_upload) = original_upload {
        if let Err(refusal) =
            crate::files_service::refuse_oversized_base64(&original_upload.data_base64)
        {
            return refusal.into_response();
        }
        match save_artifact_original_file(
            &state.app,
            &graph_id,
            &artifact_id,
            &original_upload.filename,
            &original_upload.mime_type,
            &original_upload.data_base64,
        ) {
            Ok(manifest) => {
                apply_artifact_original_manifest(&mut input, &artifact_id, &manifest);
            }
            Err(error) => return loopback_error(StatusCode::BAD_REQUEST, error.message_ref()),
        }
    }
    match enqueue_crdt_operation_outcome(
        state.app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.putArtifact".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(artifact_id.clone()),
            payload: input,
        },
    )
    .await
    {
        Ok(outcome) => {
            if let Err(error) = flush_graph_projection(state.app.clone(), &graph_id).await {
                return loopback_error(StatusCode::BAD_REQUEST, &error);
            }
            Json(outcome.value).into_response()
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_delete_navigation_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    // Files rudiments (2026-10-05): delete is recoverable. The file leaves the
    // workspace and keeps its bytes and entry in the trash until a purge
    // (`DELETE /navigation/{graph_id}/trash/{artifact_id}`). This route used
    // to remove the bytes for good.
    loopback_files_trash(state, headers, graph_id, artifact_id).await
}
