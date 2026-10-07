use crate::{
    loopback_http::{loopback_app_result, loopback_error, loopback_result, require_loopback_scope},
    loopback_state::LoopbackState,
    salience_route_service::{
        patch_local_salience_config, set_local_user_valuation, SalienceConfigPatch,
        SalienceUserValuationRequest,
    },
    salience_service::{local_block_value_scores, mcp_local_value},
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
    Json, Router,
};
use serde::Deserialize;
use std::{collections::BTreeSet, sync::Arc};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SalienceBlockValuesQuery {
    #[serde(default, alias = "document_id")]
    document_id: Option<String>,
    #[serde(default, alias = "block_id")]
    block_id: Option<String>,
    #[serde(default, alias = "block_ids")]
    block_ids: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default, alias = "min_score")]
    min_score: Option<f64>,
    #[serde(default)]
    valence: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SalienceValuationBatchRequest {
    valuations: Vec<serde_json::Value>,
}

pub(super) fn loopback_salience_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/salience/{graph_id}/blocks/values",
            get(loopback_hosted_salience_values),
        )
        .route(
            "/salience/{graph_id}/blocks/value",
            post(loopback_hosted_apply_valuations),
        )
        .route(
            "/salience/{graph_id}/blocks/user-value",
            put(loopback_hosted_user_valuation),
        )
        .route(
            "/salience/{graph_id}/config",
            patch(loopback_hosted_salience_config_patch),
        )
}

pub(super) async fn loopback_hosted_salience_values(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<SalienceBlockValuesQuery>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "salience.read") {
        return response;
    }
    let block_id_list = query.block_ids.as_ref().map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    });
    let limit = query.limit.unwrap_or(20).clamp(1, 1000);
    loopback_result(
        local_block_value_scores(
            &state.app,
            &graph_id,
            query.document_id.as_deref(),
            query.block_id.as_deref(),
            block_id_list.as_ref(),
            limit,
            query.min_score,
            query.valence.as_deref(),
        )
        .map(|blocks| {
            serde_json::json!({
                "graph_id": graph_id,
                "graphId": graph_id,
                "blocks": blocks,
                "count": blocks.len(),
            })
        }),
    )
}

pub(super) async fn loopback_hosted_apply_valuations(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<SalienceValuationBatchRequest>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "salience.write") {
        return response;
    }
    let arguments = serde_json::json!({
        "graph_id": graph_id,
        "valuations": input.valuations,
    });
    // On a graph under source authority valuations are ledger events (the
    // `value` tool's route), so a rebuild replays them. Elsewhere, exactly as
    // before, under the source gate so a concurrent first `source_pull`
    // checkpoints either before this write or after it.
    let gate = crate::source_sync::acquire_source_gate(&graph_id).await;
    if !crate::source_sync::source_authority_active(&state.app, &graph_id).unwrap_or(false) {
        let response = loopback_result(mcp_local_value(state.app.clone(), &arguments));
        drop(gate);
        return response;
    }
    drop(gate);
    loopback_app_result(
        crate::source_sync::mcp_authoritative_value(state.app.clone(), &arguments).await,
    )
}

pub(super) async fn loopback_hosted_user_valuation(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<SalienceUserValuationRequest>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "salience.write") {
        return response;
    }
    let writer = state.app.clone();
    let recorded = crate::source_sync::record_authored_write(
        &state.app,
        &graph_id,
        "userValuation",
        crate::source_sync::AuthoredScope::Values(String::new()),
        async { set_local_user_valuation(writer, &graph_id, input) },
    )
    .await;
    match recorded {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("must be one of") => {
            loopback_error(StatusCode::UNPROCESSABLE_ENTITY, &error)
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_salience_config_patch(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<SalienceConfigPatch>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "salience.write") {
        return response;
    }
    let writer = state.app.clone();
    loopback_result(
        crate::source_sync::record_authored_write(
            &state.app,
            &graph_id,
            "salienceConfig",
            crate::source_sync::AuthoredScope::Values(String::new()),
            async { patch_local_salience_config(writer, &graph_id, input) },
        )
        .await,
    )
}
