use crate::{
    hosted_entity_read_service::{
        hosted_entity_list_response, hosted_entity_response, hosted_entity_type_infos,
    },
    hosted_entity_service::{delete_hosted_entity, entity_error_response, put_hosted_entity},
    loopback_http::require_loopback_scope,
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EntityListQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EntityDeleteQuery {
    #[serde(default)]
    cascade: bool,
}

pub(super) fn loopback_entity_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/entities/entity-types", get(loopback_hosted_entity_types))
        .route(
            "/entities/{graph_id}/{entity_type}",
            get(loopback_hosted_list_entities),
        )
        .route(
            "/entities/{graph_id}/{entity_type}/{entity_id}",
            get(loopback_hosted_get_entity)
                .put(loopback_hosted_put_entity)
                .delete(loopback_hosted_delete_entity),
        )
}

pub(super) async fn loopback_hosted_entity_types(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "entities.read") {
        return response;
    }
    Json(hosted_entity_type_infos()).into_response()
}

pub(super) async fn loopback_hosted_list_entities(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, entity_type)): AxumPath<(String, String)>,
    Query(query): Query<EntityListQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "entities.read") {
        return response;
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    match hosted_entity_list_response(&state.app, &graph_id, &entity_type, limit, offset) {
        Ok(value) => Json(value).into_response(),
        Err(error) => entity_error_response(&error),
    }
}

pub(super) async fn loopback_hosted_get_entity(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, entity_type, entity_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "entities.read") {
        return response;
    }
    match hosted_entity_response(&state.app, &graph_id, &entity_type, &entity_id) {
        Ok(value) => Json(value).into_response(),
        Err(error) => entity_error_response(&error),
    }
}

pub(super) async fn loopback_hosted_put_entity(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, entity_type, entity_id)): AxumPath<(String, String, String)>,
    Json(input): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "entities.write") {
        return response;
    }
    match put_hosted_entity(state.app.clone(), graph_id, entity_type, entity_id, input).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => entity_error_response(&error),
    }
}

pub(super) async fn loopback_hosted_delete_entity(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, entity_type, entity_id)): AxumPath<(String, String, String)>,
    Query(query): Query<EntityDeleteQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "entities.delete") {
        return response;
    }
    match delete_hosted_entity(
        state.app.clone(),
        graph_id,
        entity_type,
        entity_id,
        query.cascade,
    )
    .await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => entity_error_response(&error),
    }
}
