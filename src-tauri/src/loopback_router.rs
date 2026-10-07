#[cfg(feature = "headless")]
use crate::loopback_capture_identity::capture_identity_middleware;
use crate::{
    cell_graph_boundary::cell_graph_boundary_middleware, emporium::loopback_emporium_router,
    loopback_ai_routes::loopback_ai_router, loopback_artifact_routes::loopback_artifact_router,
    loopback_core_routes::loopback_core_router, loopback_crdt_routes::loopback_crdt_router,
    loopback_document_routes::loopback_document_router,
    loopback_entity_routes::loopback_entity_router,
    loopback_files_routes::loopback_files_router, loopback_graph_routes::loopback_graph_router,
    loopback_hocuspocus_routes::loopback_hocuspocus_router,
    loopback_mcp_routes::loopback_mcp_router,
    loopback_navigation_routes::loopback_navigation_router,
    loopback_rdf_routes::loopback_rdf_router, loopback_restore_routes::loopback_restore_router,
    loopback_salience_routes::loopback_salience_router,
    loopback_service_routes::loopback_service_router, loopback_state::LoopbackState,
    loopback_time_travel_routes::loopback_time_travel_router,
    loopback_wire_routes::loopback_wire_router, runtime_config::LOCAL_UPLOAD_REQUEST_MAX_BYTES,
};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{header::RETRY_AFTER, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Json, Router,
};
use futures_util::StreamExt;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

/// Response headers a cross-origin caller must be able to READ. A browser
/// hides every non-safelisted response header from script unless the server
/// names it in `Access-Control-Expose-Headers`; `CorsLayer::very_permissive()`
/// handles the preflight and credentials but exposes nothing. The packaged
/// desktop app (origin `tauri://localhost`) fetches the loopback cross-origin,
/// so without this list `x-document-incarnation` and `x-graph-incarnation`
/// were sent and never seen: every live snapshot read as unfenced, the
/// activation cache filled with fence-less records, and the third click
/// quarantined the document ("Offline cache cannot be safely fenced",
/// 2026-09-17). Same-origin callers (the Vite dev proxy, MCP adapters) never
/// noticed, which is why the harnesses passed. Keep this list in step with
/// every `response.headers.get('x-…')` in the frontend.
pub(crate) const EXPOSED_RESPONSE_HEADERS: [&str; 4] = [
    crate::document_incarnation_store::DOCUMENT_INCARNATION_HEADER,
    crate::graph_record_store::GRAPH_INCARNATION_HEADER,
    "x-next-since",
    "x-user-id",
];

pub(crate) fn loopback_cors_layer() -> CorsLayer {
    CorsLayer::very_permissive().expose_headers(
        EXPOSED_RESPONSE_HEADERS
            .iter()
            .map(|name| axum::http::HeaderName::from_static(name))
            .collect::<Vec<_>>(),
    )
}

pub(super) fn loopback_router(state: Arc<LoopbackState>) -> Router {
    // The Tauri WebView is cross-origin: in dev it loads from Vite at
    // localhost:3000, and in packaged builds from the tauri:// or app://
    // scheme. Without CORS preflight handling, every credentialed request
    // (Authorization Bearer + X-User-ID) gets rejected at the browser.
    // `very_permissive` mirrors the request Origin and allows credentials;
    // `loopback_cors_layer` adds the response headers script must read.
    //
    // NOTE: this is NOT loopback-only. In the cell deployment the server
    // binds 0.0.0.0 (see loopback_server) and sits behind the platform-next
    // gateway, which terminates auth and ACLs upstream. The permissive CORS
    // here is acceptable because access control is enforced by the gateway,
    // and the local desktop app reaches the same surface over 127.0.0.1.
    let cors = loopback_cors_layer();

    let activity_state = state.clone();
    let cell_graph_state = state.clone();
    #[cfg(feature = "headless")]
    let capture_identity_state = state.clone();

    let router = Router::new()
        .merge(loopback_core_router())
        .merge(loopback_graph_router())
        .merge(loopback_document_router())
        .merge(loopback_entity_router())
        .merge(loopback_salience_router())
        .merge(loopback_artifact_router())
        .merge(loopback_navigation_router())
        .merge(loopback_files_router())
        .merge(loopback_ai_router())
        .merge(loopback_wire_router())
        .merge(loopback_rdf_router())
        .merge(loopback_crdt_router())
        .merge(loopback_hocuspocus_router())
        .merge(loopback_time_travel_router())
        .merge(loopback_restore_router())
        .merge(loopback_emporium_router())
        .merge(loopback_service_router())
        .merge(loopback_mcp_router())
        .layer(DefaultBodyLimit::max(LOCAL_UPLOAD_REQUEST_MAX_BYTES));

    // Observatory Capture cell spine (Phase 1): normalizes the
    // gateway-forwarded `x-sophia-capture-identity` header into request
    // scope, ONLY on requests that already pass the loopback token gate. Not
    // wired in the desktop build — `capture_event`/CaptureIdentity are
    // headless-only (this cell-shaped testimony has no desktop counterpart).
    #[cfg(feature = "headless")]
    let router = router.layer(middleware::from_fn_with_state(
        capture_identity_state,
        capture_identity_middleware,
    ));

    // The single-graph boundary sits inside CORS/activity and outside every
    // route handler. Health remains explicitly global; path, job, and
    // profile-wide decisions happen before graph state is touched.
    let router = router.layer(middleware::from_fn_with_state(
        cell_graph_state,
        cell_graph_boundary_middleware,
    ));

    // CORS remains outside the activity layer so even a draining response
    // carries the browser-visible cross-origin headers.
    router
        .layer(middleware::from_fn_with_state(
            activity_state,
            cell_activity_middleware,
        ))
        .layer(cors)
        .with_state(state)
}

async fn cell_activity_middleware(
    State(state): State<Arc<LoopbackState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // A readiness probe is evidence that the process is alive, not user
    // activity. Counting it would make Kubernetes pin every on-demand cell.
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    // A terminal enforce-mode lease can arise after the service-exposure
    // check but before Gardend's outer startup/shutdown selector observes
    // it. Refuse every non-health route in that narrow interval so a bound
    // socket can never admit a write (or falsely serve a read) for a dead
    // fencing token.
    if crate::cell_lease::handle().is_some_and(|lease| {
        lease.mode() == crate::cell_lease::LeaseMode::Enforce && lease.terminal_reason().is_some()
    }) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "lease_terminal",
                "detail": "the graph cell lost its write lease; retry through the gateway",
            })),
        )
            .into_response();
    }

    let request_lease = match state.lifecycle.admit_request() {
        Ok(lease) => lease,
        Err(_) => {
            let mut response = (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "cell_draining",
                    "detail": "the graph cell is draining; retry through the gateway",
                })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("1"));
            return response;
        }
    };
    hold_request_lease(next.run(request).await, request_lease)
}

fn hold_request_lease(
    response: Response,
    request_lease: crate::cell_lifecycle::RequestLease,
) -> Response {
    let (parts, body) = response.into_parts();
    // Handler completion is not response completion: exports and other
    // streaming bodies may still be sending bytes. Move the lease into the
    // body stream so EOF or body cancellation—not merely header creation—ends
    // the request's activity window.
    let guarded = futures_util::stream::unfold(
        (body.into_data_stream(), request_lease),
        |(mut stream, request_lease)| async move {
            stream
                .next()
                .await
                .map(|item| (item, (stream, request_lease)))
        },
    );
    Response::from_parts(parts, Body::from_stream(guarded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell_lifecycle::CellLifecycle;

    /// The desktop webview reads the incarnation headers cross-origin. A
    /// browser only lets script see them if the server names them in
    /// `Access-Control-Expose-Headers`; `very_permissive()` alone does not.
    #[tokio::test]
    async fn cross_origin_responses_expose_the_headers_the_frontend_reads() {
        use axum::{http::Request, routing::get};
        use tower::ServiceExt;

        let app = Router::new()
            .route(
                "/blob",
                get(|| async {
                    (
                        [
                            (
                                crate::document_incarnation_store::DOCUMENT_INCARNATION_HEADER,
                                "4c691f51-0eb3-4712-b3d0-9aac79213fbd",
                            ),
                            (
                                crate::graph_record_store::GRAPH_INCARNATION_HEADER,
                                "70efb31a-18ca-46c6-9780-e213bab41749",
                            ),
                        ],
                        "bytes",
                    )
                }),
            )
            .layer(loopback_cors_layer());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/blob")
                    .header("origin", "tauri://localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("tauri://localhost"),
            "origin must still be mirrored"
        );
        let exposed = headers
            .get_all("access-control-expose-headers")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",")
            .to_ascii_lowercase();
        for name in EXPOSED_RESPONSE_HEADERS {
            assert!(
                exposed.split(',').map(str::trim).any(|h| h == name),
                "{name} must be exposed to cross-origin script; got {exposed:?}"
            );
        }
        // And the headers themselves are still on the wire.
        assert!(
            headers.contains_key(crate::document_incarnation_store::DOCUMENT_INCARNATION_HEADER)
        );
        assert!(headers.contains_key(crate::graph_record_store::GRAPH_INCARNATION_HEADER));
    }

    #[tokio::test]
    async fn request_lease_lives_through_stream_eof() {
        let lifecycle = Arc::new(CellLifecycle::new());
        let lease = lifecycle.admit_request().unwrap();
        let response = hold_request_lease(Response::new(Body::from("payload")), lease);
        assert_eq!(lifecycle.snapshot().in_flight_requests, 1);

        let mut stream = response.into_body().into_data_stream();
        assert!(stream.next().await.is_some());
        assert_eq!(lifecycle.snapshot().in_flight_requests, 1);
        assert!(stream.next().await.is_none());
        assert_eq!(lifecycle.snapshot().in_flight_requests, 0);
    }
}
