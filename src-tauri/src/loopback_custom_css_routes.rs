use crate::{
    custom_css::{self, CssMutation},
    loopback_http::{loopback_app_error, require_loopback_scope},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path, State},
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn router() -> Router<Arc<LoopbackState>> {
    Router::new().route(
        "/graphs/{graph_id}/ui/custom-css",
        get(read).put(write).delete(reset),
    )
}
fn response(result: crate::app_error::AppResult<Value>) -> Response {
    let mut value = match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => loopback_app_error(error),
    };
    value
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    value
}
pub(crate) async fn read(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(graph): Path<String>,
) -> Response {
    if let Err(error) = require_loopback_scope(&headers, &state, "workspace.read") {
        return error;
    }
    response(custom_css::read(&state.app, &graph).await)
}
pub(crate) async fn write(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(graph): Path<String>,
    Json(value): Json<Value>,
) -> Response {
    if let Err(error) = require_loopback_scope(&headers, &state, "workspace.write.crdt") {
        return error;
    }
    let input: CssMutation = match serde_json::from_value(value) {
        Ok(input) => input,
        Err(error) => {
            return response(Err(crate::app_error::AppError::validation(
                error.to_string(),
            )))
        }
    };
    response(custom_css::submit(state.app.clone(), graph, input).await)
}
pub(crate) async fn reset(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(graph): Path<String>,
    Json(mut value): Json<Value>,
) -> Response {
    if value.get("cssText").is_some() {
        return response(Err(crate::app_error::AppError::validation(
            "reset must not include cssText",
        )));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert("cssText".into(), Value::String(String::new()));
    }
    write(State(state), headers, Path(graph), Json(value)).await
}
