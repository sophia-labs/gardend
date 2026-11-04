//! HTTP surface of the fold host (W7 section 4, lane T):
//!
//! - `GET  /emporium/fold/{graph_id}/{vocab}/status` the fold's watermark, lag, open and abandoned
//!   transaction runs, quarantined frames, contested count and sink, scope `rdf.query`.
//! - `GET  /emporium/fold/{graph_id}/{vocab}/heads?subject=<iri>` the head rows of one subject
//!   (value, winning frame, hlc, origin, contested), scope `rdf.query`.
//! - `POST /emporium/fold/{graph_id}/{vocab}/replay?into=oracle|live` the oracle (fold the whole
//!   ledger in memory and compare it with the sink, scope `rdf.query`) or a live rebuild of the
//!   sink (scope `rdf.update`). The default is `oracle`.
//!
//! The handlers are thin shims over the [`FoldTail`](super::tail::FoldTail) the room registry
//! starts when a ledger room opens. A read never creates a ledger: when the vocab has no ledger on
//! disk and no tail runs, `status` answers an empty body and the others answer 404. When a ledger
//! exists but is not hosted yet, the handler opens its room (which starts the tail), exactly as the
//! version route's writers do.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use super::tail::{fold_status_json, tail_for, FoldTail, ReplayInto};
use crate::app_runtime::Manager;
use crate::crdt_engine::rooms::{LedgerAdmissionError, RoomRegistry};
use crate::graph_paths::existing_graph_dir;
use crate::ids::validate_local_id;
use crate::loopback_http::{loopback_error, require_loopback_scope};
use crate::loopback_ledger_routes::ledger_dir_in;
use crate::loopback_state::LoopbackState;

const FOLD_READ_SCOPE: &str = "rdf.query";
const FOLD_WRITE_SCOPE: &str = "rdf.update";

/// The three fold routes, merged into the loopback router.
pub(crate) fn ledger_fold_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/emporium/fold/{graph_id}/{vocab}/status",
            get(loopback_fold_status),
        )
        .route(
            "/emporium/fold/{graph_id}/{vocab}/heads",
            get(loopback_fold_heads),
        )
        .route(
            "/emporium/fold/{graph_id}/{vocab}/replay",
            post(loopback_fold_replay),
        )
}

/// Validate both ids (400), then require the graph to exist (404). Returns the graph directory.
fn checked_graph(state: &LoopbackState, graph_id: &str, vocab: &str) -> Result<PathBuf, Response> {
    if let Err(message) = validate_local_id(graph_id, "graph_id") {
        return Err(loopback_error(StatusCode::BAD_REQUEST, &message));
    }
    if let Err(message) = validate_local_id(vocab, "vocab") {
        return Err(loopback_error(StatusCode::BAD_REQUEST, &message));
    }
    existing_graph_dir(&state.app, graph_id)
        .map_err(|message| loopback_error(StatusCode::NOT_FOUND, &message))
}

/// The running tail of `(graph_id, vocab)`. With none: if a ledger exists on disk its room is
/// opened (which starts the tail); if not, `Ok(None)` and nothing is created.
async fn tail_of(
    state: &LoopbackState,
    graph_id: &str,
    vocab: &str,
    graph_dir: &std::path::Path,
) -> Result<Option<Arc<FoldTail>>, Response> {
    if let Some(tail) = tail_for(graph_id, vocab) {
        return Ok(Some(tail));
    }
    if !ledger_dir_in(graph_dir, vocab).is_dir() {
        return Ok(None);
    }
    let registry = state.app.state::<RoomRegistry>();
    registry
        .open_ledger(&state.app, graph_id, vocab)
        .await
        .map_err(|error| match error {
            LedgerAdmissionError::NotFound(message) => {
                loopback_error(StatusCode::NOT_FOUND, &message)
            }
            LedgerAdmissionError::Other(message) => {
                loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &message)
            }
        })?;
    Ok(tail_for(graph_id, vocab))
}

fn tail_error(message: &str) -> Response {
    let status = if message.contains("not ready") || message.contains("stopped") {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    loopback_error(status, message)
}

/// `GET /emporium/fold/{graph_id}/{vocab}/status`. Requires `rdf.query`. A read: it never creates a
/// ledger. A vocab with no ledger answers the empty body with `running: false` (200).
pub(crate) async fn loopback_fold_status(
    Path((graph_id, vocab)): Path<(String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, FOLD_READ_SCOPE) {
        return response;
    }
    let graph_dir = match checked_graph(&state, &graph_id, &vocab) {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    if let Err(response) = tail_of(&state, &graph_id, &vocab, &graph_dir).await {
        return response;
    }
    let body = fold_status_json(&state.app, &graph_id, &vocab);
    (StatusCode::OK, Json(body)).into_response()
}

/// `GET /emporium/fold/{graph_id}/{vocab}/heads?subject=<iri>`. Requires `rdf.query`. The head rows
/// of one subject as of the tail's last cycle. 400 without `subject`, 404 without a ledger, 503
/// while the tail is still loading.
pub(crate) async fn loopback_fold_heads(
    Path((graph_id, vocab)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, FOLD_READ_SCOPE) {
        return response;
    }
    let Some(subject) = params.get("subject").filter(|s| !s.is_empty()) else {
        return loopback_error(
            StatusCode::BAD_REQUEST,
            "query parameter 'subject' is required",
        );
    };
    let graph_dir = match checked_graph(&state, &graph_id, &vocab) {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let tail = match tail_of(&state, &graph_id, &vocab, &graph_dir).await {
        Ok(Some(tail)) => tail,
        Ok(None) => {
            return loopback_error(
                StatusCode::NOT_FOUND,
                &format!("graph '{graph_id}' has no ledger for vocab '{vocab}'"),
            )
        }
        Err(response) => return response,
    };
    match tail.heads(subject).await {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(message) => tail_error(&message),
    }
}

/// `POST /emporium/fold/{graph_id}/{vocab}/replay?into=oracle|live`. `oracle` (the default) needs
/// `rdf.query`: it folds the whole ledger in memory and answers the comparison with the sink
/// (`{events, triples, equal, missing, extra, ms, ...}`), changing nothing. `live` needs
/// `rdf.update`: it rebuilds the sink from the ledger (a generation swap for a `projection:*`
/// sink, owned subjects only for `user:rdf`) and answers the comparison afterwards.
pub(crate) async fn loopback_fold_replay(
    Path((graph_id, vocab)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    let into = match params.get("into").map(String::as_str) {
        None | Some("") | Some("oracle") => ReplayInto::Oracle,
        Some("live") => ReplayInto::Live,
        Some(other) => {
            return loopback_error(
                StatusCode::BAD_REQUEST,
                &format!("'into' must be 'oracle' or 'live', not '{other}'"),
            )
        }
    };
    let scope = match into {
        ReplayInto::Oracle => FOLD_READ_SCOPE,
        ReplayInto::Live => FOLD_WRITE_SCOPE,
    };
    if let Err(response) = require_loopback_scope(&headers, &state, scope) {
        return response;
    }
    let graph_dir = match checked_graph(&state, &graph_id, &vocab) {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let tail = match tail_of(&state, &graph_id, &vocab, &graph_dir).await {
        Ok(Some(tail)) => tail,
        Ok(None) => {
            return loopback_error(
                StatusCode::NOT_FOUND,
                &format!("graph '{graph_id}' has no ledger for vocab '{vocab}'"),
            )
        }
        Err(response) => return response,
    };
    match tail.replay(into).await {
        Ok(report) => {
            let mut body = serde_json::to_value(&report).unwrap_or_else(|_| json!({}));
            if let Some(map) = body.as_object_mut() {
                map.insert("ok".to_string(), json!(true));
                map.insert(
                    "into".to_string(),
                    json!(match into {
                        ReplayInto::Oracle => "oracle",
                        ReplayInto::Live => "live",
                    }),
                );
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(message) => tail_error(&message),
    }
}
