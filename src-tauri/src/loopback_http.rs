use crate::{
    app_error::{AppError, AppErrorKind, AppResult},
    loopback_client_token_service::resolve_loopback_client_token,
    loopback_state::LoopbackState,
    runtime_config::LOOPBACK_BIND_HOST,
};
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

pub(crate) fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    origin == "null"
        || origin == format!("http://{LOOPBACK_BIND_HOST}")
        || origin.starts_with(&format!("http://{LOOPBACK_BIND_HOST}:"))
        || origin == "http://localhost"
        || origin.starts_with("http://localhost:")
        || origin == "tauri://localhost"
        || origin == "http://tauri.localhost"
}

fn loopback_token_scopes(
    headers: &HeaderMap,
    state: &LoopbackState,
) -> Result<Vec<String>, Response> {
    if !origin_ok(headers) {
        return Err(loopback_error(StatusCode::FORBIDDEN, "invalid origin"));
    }
    let Some(token) = bearer_token(headers) else {
        return Err(loopback_error(
            StatusCode::UNAUTHORIZED,
            "missing bearer token",
        ));
    };
    if token == state.token {
        return Ok(state
            .manifest
            .token_scopes
            .iter()
            .map(|scope| (*scope).to_string())
            .collect());
    }
    match resolve_loopback_client_token(&state.app, token) {
        Ok(Some(scopes)) => Ok(scopes),
        Ok(None) => Err(loopback_error(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token",
        )),
        Err(error) => Err(loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error)),
    }
}

pub(crate) fn require_loopback_scope(
    headers: &HeaderMap,
    state: &LoopbackState,
    required_scope: &str,
) -> Result<(), Response> {
    require_loopback_scopes(headers, state, &[required_scope])
}

/// True iff `headers` present the cell's own single per-run master
/// credential (`state.token`) — via either the ordinary
/// `Authorization: Bearer <token>` header, OR the
/// `Sec-WebSocket-Protocol: bearer.<token>` subprotocol used on WebSocket
/// upgrades (mirrors `loopback_hocuspocus_routes::authorize_ws`: a
/// gateway/ALB in front of this cell can strip ordinary headers on the
/// upgrade request, so browser-origin WS traffic authenticates via the
/// subprotocol instead — any identity trust gate keyed only on the
/// `Authorization` header silently never fires for WS).
///
/// Deliberately narrower than "any token `loopback_token_scopes` resolves":
/// a *scoped* client token (`loopback_client_tokens` — a different,
/// independently-issued, more widely distributable credential meant for
/// other local integrations) does NOT satisfy this check, even though it is
/// a perfectly valid loopback credential for its own scopes. This
/// function exists specifically so `loopback_capture_identity`'s trust gate
/// can require the cell's own single gateway-held secret before trusting a
/// forwarded Observatory identity — a scoped-token holder must never be
/// able to assert someone else's identity in testimony.
///
/// Also avoids `loopback_token_scopes`'s per-request `Vec<String>` scope
/// clone: this only needs a token-equality answer, never the scope list.
pub(crate) fn loopback_master_token_presented(headers: &HeaderMap, state: &LoopbackState) -> bool {
    if bearer_token(headers) == Some(state.token.as_str()) {
        return true;
    }
    let Some(protocols) = headers
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    protocols
        .split(',')
        .map(str::trim)
        .filter_map(|entry| entry.strip_prefix("bearer."))
        .any(|token| token == state.token)
}

pub(crate) fn authorized_loopback_scopes(
    headers: &HeaderMap,
    state: &LoopbackState,
    required_scopes: &[&str],
) -> Result<Vec<String>, Response> {
    let token_scopes = loopback_token_scopes(headers, state)?;
    if let Some(required_scope) = missing_required_scope(&token_scopes, required_scopes) {
        return Err(loopback_error(
            StatusCode::FORBIDDEN,
            &format!("loopback token missing required scope {required_scope}"),
        ));
    }
    Ok(token_scopes)
}

pub(crate) fn require_loopback_scopes(
    headers: &HeaderMap,
    state: &LoopbackState,
    required_scopes: &[&str],
) -> Result<(), Response> {
    authorized_loopback_scopes(headers, state, required_scopes).map(|_| ())
}

pub(crate) fn loopback_token_has_scope(
    headers: &HeaderMap,
    state: &LoopbackState,
    required_scope: &str,
) -> Result<bool, Response> {
    let token_scopes = loopback_token_scopes(headers, state)?;
    Ok(token_scope_ok(&token_scopes, required_scope))
}

fn token_scope_ok(token_scopes: &[String], required_scope: &str) -> bool {
    token_scopes
        .iter()
        .any(|candidate| candidate == required_scope)
}

fn missing_required_scope<'a>(
    token_scopes: &[String],
    required_scopes: &'a [&'a str],
) -> Option<&'a str> {
    required_scopes
        .iter()
        .copied()
        .find(|required_scope| !token_scope_ok(token_scopes, required_scope))
}

/// The REST error body, with an optional machine-readable taxonomy code
/// (`app_error_codes`). `code` is absent, never null, when unclassified — so
/// `'code' in body` is a sound support probe on the client.
pub(crate) fn loopback_error_coded(
    status: StatusCode,
    message: &str,
    code: Option<&'static str>,
) -> Response {
    let mut body = serde_json::json!({ "ok": false, "error": message });
    if let Some(code) = code {
        body["code"] = serde_json::Value::String(code.to_string());
    }
    (status, Json(body)).into_response()
}

pub(crate) fn loopback_error(status: StatusCode, message: &str) -> Response {
    if message.starts_with("source_body_unavailable:") {
        return loopback_error_coded(StatusCode::CONFLICT, message, Some(crate::app_error_codes::SOURCE_BODY_UNAVAILABLE));
    }
    loopback_error_coded(status, message, None)
}

pub(crate) fn loopback_result<T: Serialize>(result: Result<T, String>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(crate) fn loopback_app_result<T: Serialize>(result: AppResult<T>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => loopback_app_error(error),
    }
}

pub(crate) fn loopback_app_error(error: AppError) -> Response {
    loopback_error_coded(
        app_error_status(error.kind()),
        error.message_ref(),
        error.code(),
    )
}

fn app_error_status(kind: AppErrorKind) -> StatusCode {
    match kind {
        AppErrorKind::Validation => StatusCode::BAD_REQUEST,
        AppErrorKind::NotFound => StatusCode::NOT_FOUND,
        AppErrorKind::Conflict => StatusCode::CONFLICT,
        AppErrorKind::Capacity => StatusCode::SERVICE_UNAVAILABLE,
        AppErrorKind::Deadline => StatusCode::GATEWAY_TIMEOUT,
        AppErrorKind::Storage
        | AppErrorKind::Database
        | AppErrorKind::Serialization
        | AppErrorKind::Rdf
        | AppErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub(crate) fn loopback_original_file_error(error: &str) -> Response {
    if error.contains("not found")
        || error.contains("manifest.json")
        || error.contains("No such file")
    {
        loopback_error(StatusCode::NOT_FOUND, error)
    } else {
        loopback_error(StatusCode::BAD_REQUEST, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_ok_allows_loopback_and_rejects_remote_origins() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "http://127.0.0.1:1234".parse().unwrap());
        assert!(origin_ok(&headers));

        headers.insert(header::ORIGIN, "https://example.com".parse().unwrap());
        assert!(!origin_ok(&headers));
    }

    #[test]
    fn admission_failures_have_distinct_retry_semantics() {
        assert_eq!(
            app_error_status(AppErrorKind::Capacity),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            app_error_status(AppErrorKind::Deadline),
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[test]
    fn token_scope_ok_requires_exact_scope_match() {
        let scopes = vec!["documents.read".to_string(), "rdf.query".to_string()];

        assert!(token_scope_ok(&scopes, "rdf.query"));
        assert!(!token_scope_ok(&scopes, "rdf"));
        assert!(!token_scope_ok(&scopes, "rdf.update"));
    }

    #[test]
    fn missing_required_scope_reports_first_missing_scope() {
        let token_scopes = vec!["documents.read".to_string(), "rdf.query".to_string()];

        assert_eq!(
            missing_required_scope(&token_scopes, &["documents.read", "rdf.update"]),
            Some("rdf.update")
        );
        assert_eq!(
            missing_required_scope(&token_scopes, &["documents.read", "rdf.query"]),
            None
        );
        assert_eq!(missing_required_scope(&token_scopes, &[]), None);
    }

    #[test]
    fn loopback_app_result_maps_typed_error_statuses() {
        let not_found =
            loopback_app_result::<serde_json::Value>(Err(AppError::not_found("missing")));
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

        let conflict = loopback_app_result::<serde_json::Value>(Err(AppError::conflict("exists")));
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let storage =
            loopback_app_result::<serde_json::Value>(Err(AppError::storage("disk failed")));
        assert_eq!(storage.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn loopback_app_error_carries_the_taxonomy_code() {
        let coded = loopback_app_error(
            AppError::conflict("stale graph incarnation for g: expected a, actual b")
                .with_code(crate::app_error_codes::STALE_GRAPH_INCARNATION),
        );
        assert_eq!(coded.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(coded.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["code"], "stale_graph_incarnation");

        let uncoded = loopback_app_error(AppError::conflict("plain conflict, no taxonomy"));
        let body = axum::body::to_bytes(uncoded.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            value.get("code").is_none(),
            "an unclassified error carries no `code` key at all — absent, never null"
        );
    }
}
