//! `POST /emporium/ingest/{graph_id}` — the ingest endpoint.
//!
//! Receive → validate (shape) → survey (read-only) → plan → (apply). `graph_id`
//! is a URL PATH param, which deliberately bypasses the MCP-sparql camelCase
//! `graphId` seam (the body never carries a graphId).
//!
//! Scope:
//! - accept + validate the JSON payload (the typed inbox, `schemas.rs`);
//! - `dry_run=true`: run the read-only spine ([`spine::gather_and_plan`]) — the
//!   survey over `GRAPH <{root}:user:rdf>` (read==write graph) + workspace
//!   snapshot reads, then the PURE planner — and return the full [`Plan`]
//!   (graph/workflow/mode/shortId/workflowDocId/steps/summary/warnings);
//! - `dry_run=false`: spine plan, then the loud-halt CRDT applier
//!   ([`applier::apply_plan`]) — maps each plan verb to gardend's in-process CRDT
//!   surface, folds RDF + emp: provenance into `GRAPH <{root}:user:rdf>` via
//!   `graph_wrap` + `run_sparql_update_service` (read==write; the validator BANS
//!   the bare root + :projection:). Returns the apply report; a halted step
//!   surfaces as a 422 with `ok=false` + `haltedAt`.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
#[cfg(test)]
use serde::Serialize;

use crate::app_error::AppError;
use crate::emporium::schemas::IngestRequest;
use crate::emporium::spine::{apply_and_assert, gather_and_plan};
#[cfg(test)]
use crate::emporium::survey::Live;
use crate::loopback_http::{loopback_app_error, loopback_error, require_loopback_scope};
use crate::loopback_state::LoopbackState;

/// The surveyed counts that project from a [`Live`] snapshot (read-graph anchor
/// + entity tallies). Retained as a test fixture / projection helper now that the
/// apply path returns the applier report instead of an ACK envelope.
#[cfg(test)]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SurveyCounts {
    pub(crate) read_graph: String,
    pub(crate) workflows: usize,
    pub(crate) archetypes: usize,
    pub(crate) contracts: usize,
    pub(crate) runs: usize,
    pub(crate) nodes: usize,
}

#[cfg(test)]
impl From<&Live> for SurveyCounts {
    fn from(live: &Live) -> Self {
        let nodes = live.nodes_by_workflow.values().map(|m| m.len()).sum();
        SurveyCounts {
            read_graph: live.read_graph.clone(),
            workflows: live.workflows.len(),
            archetypes: live.archetypes.len(),
            contracts: live.contracts.len(),
            runs: live.runs.len(),
            nodes,
        }
    }
}

/// `POST /emporium/ingest/{graph_id}` handler.
pub(crate) async fn ingest_handler(
    Path(graph_id): Path<String>,
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    body: Result<Json<IngestRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // The vocab routes are intentionally auth-free (they only serve the
    // sha-pinned golden bytes). This route MUTATES the cell, so — unlike the
    // gateway, which is the only guard in the remote path — it carries its own
    // scope guard for the local/desktop loopback surface, matching the other
    // mutating RDF loopback routes (`loopback_rdf_routes`). The read-only
    // dry_run path requires the `rdf.query` read scope; the apply path requires
    // the `rdf.update` write scope. Payload shape is validated only after the
    // caller clears the scope gate.
    let Json(request) = match body {
        Ok(json) => json,
        Err(rejection) => {
            return loopback_error(
                StatusCode::BAD_REQUEST,
                &format!("invalid ingest payload: {rejection}"),
            );
        }
    };

    let required_scope = if request.dry_run {
        "rdf.query"
    } else {
        "rdf.update"
    };
    if let Err(response) = require_loopback_scope(&headers, &state, required_scope) {
        return response;
    }

    if let Err(message) = request.validate() {
        return loopback_error(StatusCode::BAD_REQUEST, &message);
    }

    // dry_run=true → the read-only spine: survey + workspace snapshot reads, then
    // the PURE planner. Return the full plan (graph/workflow/mode/shortId/
    // workflowDocId/steps/summary/warnings). No writes.
    if request.dry_run {
        return match gather_and_plan(&state.app, &graph_id, &request) {
            Ok(planned) => (StatusCode::OK, Json(planned.plan)).into_response(),
            Err(error) => ingest_error_response(error),
        };
    }

    // dry_run=false → the full apply path: survey + plan (the spine), then run
    // the loud-halt CRDT applier and — for the workflow kind — fold in the
    // post-apply re-survey + assertion. Returns the apply report
    // ({ok, steps, summary, warnings, haltedAt?, assertion}).
    //
    // Per-graph write gate: survey→plan→apply is a read-modify-write; hold the
    // gate across both phases so concurrent mutating ingests serialize (the
    // dry-run path above is read-only and deliberately ungated).
    //
    // On a graph under source authority the apply is recorded in the source
    // ledger (`source_sync::record_authored_write`), so a rebuild replays it;
    // the source gate is taken before the write gate. Unchanged elsewhere.
    let recorded = crate::source_sync::record_authored_write(
        &state.app,
        &graph_id,
        "emporiumIngest",
        crate::source_sync::AuthoredScope::Authored,
        async {
            let _write_gate = crate::emporium::write_gate::acquire_write_gate(&graph_id).await;
            let planned = match gather_and_plan(&state.app, &graph_id, &request) {
                Ok(planned) => planned,
                Err(error) => return Ok::<Response, AppError>(ingest_error_response(error)),
            };
            // The contract is now PLAN-DERIVED (the spine selected wf vs memory). The
            // apply-dispatch fork inside `apply_and_assert` routes by `plan.vocab`, so the
            // memory kind is not forced through the wf contract/applier.
            let report = apply_and_assert(
                &state.app,
                &graph_id,
                &planned.plan,
                planned.contract,
                &request,
            )
            .await;
            let status = if report.ok {
                StatusCode::OK
            } else {
                // A loud halt is a 422: the request was well-formed but a step failed.
                StatusCode::UNPROCESSABLE_ENTITY
            };
            Ok((status, Json(report)).into_response())
        },
    )
    .await;
    match recorded {
        Ok(response) => response,
        Err(error) => loopback_app_error(error),
    }
}

/// Map a survey error to a clean HTTP response. `existing_graph_dir` failure on
/// a non-existent graph arrives as a storage error; we surface it as a 404 so
/// the route never panics on an unknown graph.
fn ingest_error_response(error: AppError) -> Response {
    let message = error.message_ref();
    if message.contains("not found") || message.contains("No such file") {
        return loopback_error(StatusCode::NOT_FOUND, message);
    }
    loopback_app_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::survey::EntityRow;

    fn live_with(workflows: usize, runs: usize) -> Live {
        let mut live = Live {
            graph: "lab".into(),
            prefix: "urn:mnemosyne:local:graph:lab".into(),
            read_graph: "urn:mnemosyne:local:graph:lab:user:rdf".into(),
            ..Live::default()
        };
        for i in 0..workflows {
            live.workflows.insert(
                format!("wf{i}"),
                EntityRow {
                    uri: format!("urn:mnemosyne:local:graph:lab:doc:wf{i}"),
                    doc_id: Some(format!("wf{i}")),
                    name: format!("wf{i}"),
                    sha: None,
                },
            );
        }
        live.runs = (0..runs).map(|i| format!("run-{i}")).collect();
        live
    }

    #[test]
    fn survey_counts_project_from_live() {
        let live = live_with(2, 3);
        let counts = SurveyCounts::from(&live);
        assert_eq!(counts.workflows, 2);
        assert_eq!(counts.runs, 3);
        assert_eq!(counts.read_graph, "urn:mnemosyne:local:graph:lab:user:rdf");
    }
}
