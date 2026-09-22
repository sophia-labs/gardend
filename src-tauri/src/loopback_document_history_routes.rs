use crate::{
    document_history_service::{
        copy_document_snapshot_as_manual, current_document_snapshot,
        delete_manual_document_snapshot, document_snapshot_hosted_json,
        hosted_document_snapshot_count_response, hosted_document_snapshot_html_response,
        hosted_document_snapshot_list_response, hosted_document_snapshot_text_response,
    },
    loopback_document_inputs::ManualSnapshotRequest,
    loopback_http::{loopback_error, require_loopback_scope},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::{collections::BTreeMap, sync::Arc};

fn legacy_history_catalog(state: &LoopbackState, graph: &str)
    -> Result<(std::path::PathBuf, serde_json::Value), Response> {
    let lease=crate::cell_graph_boundary::current_cell_lease()
        .ok_or_else(||loopback_error(StatusCode::FORBIDDEN,"verified source-owner lease required"))?;
    let owner=state.cell_graph.owner_principal()
        .ok_or_else(||loopback_error(StatusCode::FORBIDDEN,"owner-bound cell required"))?;
    if lease.role!=crate::cell_graph_boundary::CellRole::Owner || lease.principal!=owner
        || state.manifest.cell_graph_id.as_deref()!=Some(graph) {
        return Err(loopback_error(StatusCode::FORBIDDEN,"source-owner lease required"));
    }
    let source_owner=owner.strip_prefix("user:")
        .ok_or_else(||loopback_error(StatusCode::FORBIDDEN,"source-owner principal required"))?;
    let dir=crate::paths::existing_graph_dir(&state.app,graph).map_err(legacy_history_error)?;
    let catalog=crate::document_legacy_history::catalog(&dir,graph,source_owner).map_err(legacy_history_error)?;
    Ok((dir,catalog))
}

fn legacy_history_error(error: String) -> Response {
    let status=if error.contains("byte budget") {StatusCode::PAYLOAD_TOO_LARGE}
        else if error=="source-payload-unavailable" {StatusCode::GONE}
        else if error.contains("not found"){StatusCode::NOT_FOUND}else{StatusCode::BAD_REQUEST};
    loopback_error(status,&error)
}

fn legacy_metadata_response(value: &serde_json::Value) -> Response {
    match crate::document_legacy_history::metadata_bytes(value) {
        Ok(bytes)=>Response::builder().status(StatusCode::OK).header("content-type","application/json")
            .header("cache-control","private, no-store").header("x-content-type-options","nosniff")
            .header("content-length",bytes.len()).body(axum::body::Body::from(bytes))
            .unwrap_or_else(|_|loopback_error(StatusCode::INTERNAL_SERVER_ERROR,"legacy metadata response failed")),
        Err(error)=>legacy_history_error(error),
    }
}

pub(super) async fn loopback_legacy_history_list(State(state):State<Arc<LoopbackState>>,
    headers:HeaderMap, AxumPath(params):AxumPath<BTreeMap<String,String>>,
    Query(query):Query<BTreeMap<String,String>>) -> Response {
    if let Err(response)=require_loopback_scope(&headers,&state,"documents.history.read"){return response}
    let Some(graph)=params.get("graph_id") else{return loopback_error(StatusCode::BAD_REQUEST,"graph missing")};
    match legacy_history_catalog(&state,graph) {
        Ok((_,mut catalog))=>{
            if let Some(doc)=params.get("doc_id") {
                if let Some(rows)=catalog["entries"].as_array_mut(){rows.retain(|row|row["documentId"]==*doc)}
                // Global counts remain explicitly named; filtered entries are not the complete partition.
                catalog["documentFilter"]=serde_json::json!(doc);
            }
            let limit=match query.get("limit").map(|s|s.parse::<usize>()).transpose() {
                Ok(limit)=>limit.unwrap_or(100).clamp(1,200),Err(_)=>return loopback_error(StatusCode::BAD_REQUEST,"invalid legacy history limit")};
            let cursor=match query.get("cursor").map(|s|s.parse::<usize>()).transpose() {
                Ok(cursor)=>cursor.unwrap_or(0),Err(_)=>return loopback_error(StatusCode::BAD_REQUEST,"invalid legacy history cursor")};
            let rows=catalog["entries"].as_array_mut().expect("validated history entries");
            if cursor>rows.len(){return loopback_error(StatusCode::BAD_REQUEST,"legacy history cursor out of range")}
            let end=cursor.saturating_add(limit).min(rows.len());let next=(end<rows.len()).then_some(end);
            *rows=rows[cursor..end].to_vec();
            catalog["page"]=serde_json::json!({"cursor":cursor,"limit":limit,"nextCursor":next});
            legacy_metadata_response(&catalog)
        },
        Err(response)=>response,
    }
}

pub(super) async fn loopback_legacy_history_get(State(state):State<Arc<LoopbackState>>,
    headers:HeaderMap, AxumPath(params):AxumPath<BTreeMap<String,String>>) -> Response {
    if let Err(response)=require_loopback_scope(&headers,&state,"documents.history.read"){return response}
    let Some(graph)=params.get("graph_id") else{return loopback_error(StatusCode::BAD_REQUEST,"graph missing")};
    let Some(doc)=params.get("doc_id") else{return loopback_error(StatusCode::BAD_REQUEST,"document missing")};
    let Some(id)=params.get("snapshot_id") else{return loopback_error(StatusCode::BAD_REQUEST,"snapshot missing")};
    let (dir,catalog)=match legacy_history_catalog(&state,graph){Ok(v)=>v,Err(r)=>return r};
    let row=match crate::document_legacy_history::entry(&catalog,doc,id){Ok(v)=>v,Err(e)=>return legacy_history_error(e)};
    let Some(view)=params.get("view") else{return legacy_metadata_response(&row)};
    if view!="text" && view!="download" {return loopback_error(StatusCode::NOT_FOUND,"legacy history view not found")}
    let bytes=match crate::document_legacy_history::source_bytes(&dir,&row){Ok(v)=>v,Err(e)=>return legacy_history_error(e)};
    let (bytes,mime)=if view=="text" {
        match crate::document_legacy_history::literal_text(&bytes){
            Ok(text)=>(text.into_bytes(),"text/plain; charset=utf-8"),Err(e)=>return legacy_history_error(e)}
    }else{(bytes,"application/json")};
    let mut response=Response::builder().status(StatusCode::OK)
        .header("content-type",mime).header("x-content-type-options","nosniff")
        .header("cache-control","private, no-store")
        .header("content-security-policy","default-src 'none'; sandbox")
        .header("content-length",bytes.len());
    if view=="download" {response=response.header("content-disposition","attachment; filename=legacy-history.json")}
    response.body(axum::body::Body::from(bytes)).unwrap_or_else(|_|loopback_error(StatusCode::INTERNAL_SERVER_ERROR,"legacy response failed"))
}

pub(super) async fn loopback_hosted_list_document_snapshots(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id)): AxumPath<(String, String)>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.history.read") {
        return response;
    }
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(200);
    match hosted_document_snapshot_list_response(&state.app, &graph_id, &doc_id, limit) {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_create_manual_document_snapshot(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id)): AxumPath<(String, String)>,
    body: Option<Json<ManualSnapshotRequest>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.snapshots.write") {
        return response;
    }
    let label = body
        .and_then(|Json(input)| input.label)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match current_document_snapshot(&state.app, &graph_id, &doc_id, true, label) {
        Ok(snapshot) => (
            StatusCode::CREATED,
            Json(document_snapshot_hosted_json(&snapshot)),
        )
            .into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_document_snapshot_count(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.history.read") {
        return response;
    }
    match hosted_document_snapshot_count_response(&state.app, &graph_id, &doc_id) {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_document_snapshot_html(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id, snapshot_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.history.read") {
        return response;
    }
    match hosted_document_snapshot_html_response(&state.app, &graph_id, &doc_id, &snapshot_id) {
        Ok(response) => response,
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_document_snapshot_text(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id, snapshot_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.history.read") {
        return response;
    }
    match hosted_document_snapshot_text_response(&state.app, &graph_id, &doc_id, &snapshot_id) {
        Ok(response) => response,
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_copy_document_snapshot(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id, snapshot_id)): AxumPath<(String, String, String)>,
    body: Option<Json<ManualSnapshotRequest>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.snapshots.write") {
        return response;
    }
    let label = body
        .and_then(|Json(input)| input.label)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match copy_document_snapshot_as_manual(&state.app, &graph_id, &doc_id, &snapshot_id, label) {
        Ok(snapshot) => (
            StatusCode::CREATED,
            Json(document_snapshot_hosted_json(&snapshot)),
        )
            .into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_delete_document_snapshot(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id, snapshot_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.snapshots.delete") {
        return response;
    }
    match delete_manual_document_snapshot(&state.app, &graph_id, &doc_id, &snapshot_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) if error.contains("manually saved") => {
            loopback_error(StatusCode::FORBIDDEN, &error)
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}
