use crate::{
    app_error::{AppError, AppErrorKind, AppResult},
    loopback_client_token_service::resolve_loopback_client_token_record,
    loopback_self_host::{AccessGrant, AccessIdentity},
    loopback_state::LoopbackState,
    loopback_token_grants::{grant_profile_scopes, MEMBER_GRANT_PROFILE},
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
    origin == "null" || local_origin(origin)
}

/// A loopback or Tauri webview origin (the desktop app, the Vite dev server,
/// a same-machine browser). `null` is deliberately not one.
pub(crate) fn local_origin(origin: &str) -> bool {
    origin == format!("http://{LOOPBACK_BIND_HOST}")
        || origin.starts_with(&format!("http://{LOOPBACK_BIND_HOST}:"))
        || origin == "http://localhost"
        || origin.starts_with("http://localhost:")
        || origin == "tauri://localhost"
        || origin == "http://tauri.localhost"
}

/// [`origin_ok`] plus the self-host allowed origins (`GARDEN_ALLOWED_ORIGINS`,
/// or the public base URL's origin under Cloudflare Access). With no
/// self-host knob set the list is empty and this is exactly [`origin_ok`].
pub(crate) fn origin_allowed(headers: &HeaderMap, state: &LoopbackState) -> bool {
    if origin_ok(headers) {
        return true;
    }
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| state.self_host.origin_listed(origin))
}

/// Who a loopback request speaks for, as its credential proves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopbackPrincipal {
    /// The master key (`state.token`; under `gardend serve`, the profile's key).
    Owner,
    /// A client token from the profile's token store (`gardend token create`).
    ClientToken { token_id: String },
    /// A local service gardend supervises, with the MCP token gardend minted
    /// for that process (`local_service_host`).
    LocalService { service_id: String },
    /// A person Cloudflare Access admitted (self-host).
    AccessPerson { email: String },
    /// A Cloudflare Access service token (self-host).
    AccessService { client_id: String },
    /// A caller with no credential, on a gardend in public mode: `anon:<hash>`
    /// (`public_mode::visitor_id`). A viewer of the public graphs only.
    Visitor { id: String },
    /// A person signed in with a Sophia account (a Cognito ID token), on a
    /// gardend in public mode. A viewer of the public graphs only.
    Account {
        sub: String,
        email: Option<String>,
        email_verified: bool,
    },
}

impl LoopbackPrincipal {
    /// The user id a local service is told the caller is (`X-User-ID`):
    /// `owner`, `token:<id>`, `service:<id>`, the Access email, or
    /// `access-service:<client id>`. Kept to the characters Choreograph
    /// accepts in a `user:` principal (`[A-Za-z0-9_.@:-]`, at most 192).
    pub(crate) fn user_id(&self) -> String {
        let raw = match self {
            Self::Owner => "owner".to_string(),
            Self::ClientToken { token_id } => format!("token:{token_id}"),
            Self::LocalService { service_id } => format!("service:{service_id}"),
            Self::AccessPerson { email } => email.to_ascii_lowercase(),
            Self::AccessService { client_id } => format!("access-service:{client_id}"),
            Self::Visitor { id } => id.clone(),
            Self::Account { sub, .. } => format!("cognito:{sub}"),
        };
        let mut id: String = raw
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '@' | ':' | '-') {
                    ch
                } else {
                    '_'
                }
            })
            .take(192)
            .collect();
        if id.is_empty() {
            id.push_str("unknown");
        }
        id
    }
}

/// An authenticated loopback caller: who it is and the scopes it holds.
#[derive(Debug, Clone)]
pub(crate) struct LoopbackCaller {
    pub(crate) principal: LoopbackPrincipal,
    pub(crate) scopes: Vec<String>,
}

impl LoopbackCaller {
    pub(crate) fn holds(&self, scope: &str) -> bool {
        token_scope_ok(&self.scopes, scope)
    }

    /// A public-mode caller: an anonymous visitor or a Sophia account.
    pub(crate) fn is_public(&self) -> bool {
        matches!(
            self.principal,
            LoopbackPrincipal::Visitor { .. } | LoopbackPrincipal::Account { .. }
        )
    }
}

fn public_scopes() -> Vec<String> {
    crate::public_mode::PUBLIC_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect()
}

/// The anonymous visitor of a request in public mode (`None` otherwise, and
/// for any request that carries an account token).
fn public_visitor_caller(state: &LoopbackState) -> Option<LoopbackCaller> {
    state.self_host.public()?;
    let request = crate::public_mode::current_public_request()?;
    if request.account_token {
        return None;
    }
    Some(LoopbackCaller {
        principal: LoopbackPrincipal::Visitor {
            id: request.visitor_id,
        },
        scopes: public_scopes(),
    })
}

/// A Sophia account's ID token, in public mode with accounts configured:
/// `None` when it is neither (the caller's own answer stands), else the
/// account or a 401.
fn public_account_caller(
    token: &str,
    state: &LoopbackState,
) -> Option<Result<LoopbackCaller, Response>> {
    let accounts = state.self_host.public()?.accounts()?;
    if !crate::public_mode::looks_like_jwt(token) {
        return None;
    }
    Some(match accounts.verify(token) {
        Ok(account) => {
            // W6: a route the account's fence admitted lends it that route's
            // scopes, for this one request (`visitor_fence::rest_admission`).
            let mut scopes = public_scopes();
            for scope in
                crate::visitor_fence::current_lent_scopes(&format!("cognito:{}", account.sub))
            {
                if !scopes.contains(&scope) {
                    scopes.push(scope);
                }
            }
            Ok(LoopbackCaller {
                principal: LoopbackPrincipal::Account {
                    sub: account.sub,
                    email: account.email,
                    email_verified: account.email_verified,
                },
                scopes,
            })
        }
        Err(error) => {
            log::debug!("public mode: refused an account token: {error}");
            Err(loopback_error(
                StatusCode::UNAUTHORIZED,
                "invalid or expired Sophia account token (sign in again)",
            ))
        }
    })
}

fn loopback_token_scopes(
    headers: &HeaderMap,
    state: &LoopbackState,
) -> Result<Vec<String>, Response> {
    loopback_caller(headers, state).map(|caller| caller.scopes)
}

/// Authenticate a request: the master key, a supervised service's own token,
/// a client token, else a Cloudflare Access login (self-host), in that order.
pub(crate) fn loopback_caller(
    headers: &HeaderMap,
    state: &LoopbackState,
) -> Result<LoopbackCaller, Response> {
    let Some(token) = bearer_token(headers) else {
        if !origin_allowed(headers, state) {
            // A visitor of a public Sophia carries no ambient credential, so
            // any page may read what is public anyway.
            if let Some(caller) = public_visitor_caller(state) {
                return Ok(caller);
            }
            return Err(loopback_error(StatusCode::FORBIDDEN, "invalid origin"));
        }
        if let Some(caller) = access_login_caller(headers, state)? {
            return Ok(caller);
        }
        if let Some(caller) = public_visitor_caller(state) {
            return Ok(caller);
        }
        return Err(loopback_error(
            StatusCode::UNAUTHORIZED,
            "missing bearer token",
        ));
    };
    if !origin_allowed(headers, state) {
        // A Sophia account's token is a bearer (never ambient): it is good
        // from any page.
        if let Some(result) = public_account_caller(token, state) {
            return result;
        }
        return Err(loopback_error(StatusCode::FORBIDDEN, "invalid origin"));
    }
    if token == state.token {
        return Ok(LoopbackCaller {
            principal: LoopbackPrincipal::Owner,
            scopes: state
                .manifest
                .token_scopes
                .iter()
                .map(|scope| (*scope).to_string())
                .collect(),
        });
    }
    if let Some(service_id) = state.services.service_token_owner(token) {
        return Ok(LoopbackCaller {
            principal: LoopbackPrincipal::LocalService { service_id },
            scopes: grant_profile_scopes(MEMBER_GRANT_PROFILE),
        });
    }
    match resolve_loopback_client_token_record(&state.app, token) {
        Ok(Some(record)) => Ok(LoopbackCaller {
            principal: LoopbackPrincipal::ClientToken {
                token_id: record.token_id,
            },
            scopes: record.scopes,
        }),
        Ok(None) => {
            if let Some(result) = public_account_caller(token, state) {
                return result;
            }
            if let Some(caller) = access_login_caller(headers, state)? {
                return Ok(caller);
            }
            Err(loopback_error(
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token",
            ))
        }
        Err(error) => Err(loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error)),
    }
}

/// True when the request carries a verified Cloudflare Access login (from an
/// allowed origin) whose grant includes `scope`. Always false unless Access is
/// configured.
pub(crate) fn access_login_grants(headers: &HeaderMap, state: &LoopbackState, scope: &str) -> bool {
    matches!(
        access_login_caller(headers, state),
        Ok(Some(caller)) if caller.holds(scope)
    )
}

/// A Cloudflare Access login (self-host only). `Ok(None)`: no Access login on
/// this request (Access is not configured, or no `Cf-Access-Jwt-Assertion`),
/// so the caller's own answer stands. A token that does not verify is 401; a
/// verified token on a request whose origin the ambient-credential rule
/// refuses is 403.
fn access_login_caller(
    headers: &HeaderMap,
    state: &LoopbackState,
) -> Result<Option<LoopbackCaller>, Response> {
    match state.self_host.access_grant(headers) {
        None => Ok(None),
        Some(Ok(grant)) => {
            if !state.self_host.ambient_origin_ok(headers) {
                return Err(loopback_error(
                    StatusCode::FORBIDDEN,
                    "invalid origin for a Cloudflare Access login",
                ));
            }
            Ok(Some(access_caller(&grant)))
        }
        Some(Err(error)) => {
            log::debug!("self-host: refused Cloudflare Access token: {error}");
            Err(loopback_error(
                StatusCode::UNAUTHORIZED,
                "invalid Cloudflare Access token",
            ))
        }
    }
}

fn access_caller(grant: &AccessGrant) -> LoopbackCaller {
    LoopbackCaller {
        principal: match &grant.identity {
            AccessIdentity::Person { email } => LoopbackPrincipal::AccessPerson {
                email: email.clone(),
            },
            AccessIdentity::Service { client_id } => LoopbackPrincipal::AccessService {
                client_id: client_id.clone(),
            },
        },
        scopes: grant.scopes(),
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
    authorized_loopback_caller(headers, state, required_scopes).map(|caller| caller.scopes)
}

/// [`loopback_caller`] holding every one of `required_scopes`, else 403.
pub(crate) fn authorized_loopback_caller(
    headers: &HeaderMap,
    state: &LoopbackState,
    required_scopes: &[&str],
) -> Result<LoopbackCaller, Response> {
    let caller = loopback_caller(headers, state)?;
    if let Some(required_scope) = missing_required_scope(&caller.scopes, required_scopes) {
        return Err(loopback_error(
            StatusCode::FORBIDDEN,
            &format!("loopback token missing required scope {required_scope}"),
        ));
    }
    Ok(caller)
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
        return loopback_error_coded(
            StatusCode::CONFLICT,
            message,
            Some(crate::app_error_codes::SOURCE_BODY_UNAVAILABLE),
        );
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
    fn principals_map_to_user_ids_choreograph_accepts() {
        assert_eq!(LoopbackPrincipal::Owner.user_id(), "owner");
        assert_eq!(
            LoopbackPrincipal::ClientToken {
                token_id: "3f2a9c1e-0000-4000-8000-000000000001".into()
            }
            .user_id(),
            "token:3f2a9c1e-0000-4000-8000-000000000001"
        );
        assert_eq!(
            LoopbackPrincipal::LocalService {
                service_id: "choreograph".into()
            }
            .user_id(),
            "service:choreograph"
        );
        assert_eq!(
            LoopbackPrincipal::AccessPerson {
                email: "Lucy+Sophia@Example.com".into()
            }
            .user_id(),
            "lucy_sophia@example.com",
            "lowercased, and `+` is outside Choreograph's principal alphabet"
        );
        assert_eq!(
            LoopbackPrincipal::AccessService {
                client_id: "6fe0d2a1.access".into()
            }
            .user_id(),
            "access-service:6fe0d2a1.access"
        );
        let long = LoopbackPrincipal::AccessPerson {
            email: format!("{}@example.com", "a".repeat(400)),
        };
        assert_eq!(long.user_id().len(), 192);
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
