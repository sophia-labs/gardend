use crate::app_runtime::AppHandle;
use crate::{
    clock::duration_ms,
    crdt_queue::{
        enqueue_crdt_operation, enqueue_crdt_operation_outcome_marked, CrdtOperationQueue,
        EnqueueCrdtOperationInput,
    },
    loopback_audit_log::{append_loopback_audit_event, LoopbackAuditEvent},
};
use axum::http::HeaderMap;
use std::{
    sync::{atomic::AtomicBool, Arc},
    time::Instant,
};
#[cfg(feature = "desktop")]
use tauri::Manager;

/// Header callers send to opt back into the legacy synchronous flush path
/// (read-after-write fidelity at the cost of latency).
pub(crate) const WAIT_FOR_FLUSH_PREFERENCE: &str = "wait-for-flush";

pub(crate) fn prefers_synchronous_flush(headers: &HeaderMap) -> bool {
    headers
        .get("prefer")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(WAIT_FOR_FLUSH_PREFERENCE))
        })
        .unwrap_or(false)
}

pub(crate) async fn flush_graph_projection(app: AppHandle, graph_id: &str) -> Result<(), String> {
    flush_projection_with_phase(app, graph_id, None, None, None, None, None).await
}

/// Keep a caller's lifetime fence on the post-operation projection tail.
pub(crate) async fn flush_projection_incarnation(
    app: AppHandle,
    graph_id: &str,
    document_id: Option<&str>,
    expected_incarnation: &str,
    phase: Option<(&str, &str)>,
) -> Result<(), String> {
    let started = Instant::now();
    enqueue_crdt_operation(app.clone(), EnqueueCrdtOperationInput {
        kind: "crdt.flush".into(), graph_id: graph_id.to_string(),
        document_id: document_id.map(str::to_string),
        payload: serde_json::json!({"includeMaterialization":true,"graphIncarnation":expected_incarnation}),
    }).await?;
    if let Some((operation_id, phase)) = phase {
        let _ = app.state::<CrdtOperationQueue>().add_phase(operation_id, phase, duration_ms(started.elapsed()));
    }
    Ok(())
}

pub(crate) async fn flush_document_projection(
    app: AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<(), String> {
    flush_projection_with_phase(app, graph_id, Some(document_id), None, None, None, None).await
}

pub(crate) async fn flush_graph_projection_generation(
    app: AppHandle,
    graph_id: &str,
    graph_generation: u64,
    queued: Arc<AtomicBool>,
) -> Result<(), String> {
    flush_projection_with_phase(
        app,
        graph_id,
        None,
        None,
        None,
        Some(graph_generation),
        Some(queued),
    )
    .await
}

pub(crate) async fn flush_document_projection_generation(
    app: AppHandle,
    graph_id: &str,
    document_id: &str,
    graph_generation: u64,
    queued: Arc<AtomicBool>,
) -> Result<(), String> {
    flush_projection_with_phase(
        app,
        graph_id,
        Some(document_id),
        None,
        None,
        Some(graph_generation),
        Some(queued),
    )
    .await
}

pub(crate) async fn flush_graph_projection_phase(
    app: AppHandle,
    graph_id: &str,
    operation_id: &str,
    phase: &str,
) -> Result<(), String> {
    flush_projection_with_phase(
        app,
        graph_id,
        None,
        Some(operation_id),
        Some(phase),
        None,
        None,
    )
    .await
}

pub(crate) async fn flush_document_projection_phase(
    app: AppHandle,
    graph_id: &str,
    document_id: &str,
    operation_id: &str,
    phase: &str,
) -> Result<(), String> {
    flush_projection_with_phase(
        app,
        graph_id,
        Some(document_id),
        Some(operation_id),
        Some(phase),
        None,
        None,
    )
    .await
}

async fn flush_projection_with_phase(
    app: AppHandle,
    graph_id: &str,
    document_id: Option<&str>,
    operation_id: Option<&str>,
    phase: Option<&str>,
    graph_generation: Option<u64>,
    queued: Option<Arc<AtomicBool>>,
) -> Result<(), String> {
    let started = Instant::now();
    let input = EnqueueCrdtOperationInput {
        kind: "crdt.flush".to_string(),
        graph_id: graph_id.to_string(),
        document_id: document_id.map(str::to_string),
        payload: serde_json::json!({
            "includeMaterialization": true,
            "graphGeneration": graph_generation,
        }),
    };
    if let Some(queued) = queued {
        enqueue_crdt_operation_outcome_marked(app.clone(), input, queued).await?;
    } else {
        enqueue_crdt_operation(app.clone(), input).await?;
    }

    if let (Some(operation_id), Some(phase)) = (operation_id, phase) {
        let queue = app.state::<CrdtOperationQueue>();
        let _ = queue.add_phase(operation_id, phase, duration_ms(started.elapsed()));
    }

    Ok(())
}

/// Schedule a document-scoped projection flush off the request path.
/// The originating request returns once the upstream CRDT enqueue succeeds;
/// any failure of the deferred flush is recorded to the loopback audit log.
pub(crate) fn spawn_deferred_document_projection_flush(
    app: AppHandle,
    graph_id: String,
    document_id: String,
    route: &'static str,
) {
    tokio::spawn(async move {
        if let Err(error) = flush_projection_with_phase(
            app.clone(),
            &graph_id,
            Some(&document_id),
            None,
            None,
            None,
            None,
        )
        .await
        {
            log_deferred_flush_failure(&app, route, &graph_id, Some(&document_id), &error);
        }
    });
}

fn log_deferred_flush_failure(
    app: &AppHandle,
    route: &str,
    graph_id: &str,
    document_id: Option<&str>,
    error: &str,
) {
    let event = LoopbackAuditEvent::new(
        "crdt.flush.deferred",
        route,
        "local-runtime",
        "failed",
        document_id.map(str::to_string),
        serde_json::json!({
            "route": route,
            "graphId": graph_id,
            "documentId": document_id,
            "error": error,
        }),
    );
    if let Err(audit_error) = append_loopback_audit_event(app, &event) {
        log::error!(
            "deferred crdt.flush failure for route {route} graph {graph_id}: {error} \
             (audit append failed: {audit_error})"
        );
    } else {
        log::warn!("deferred crdt.flush failure for route {route} graph {graph_id}: {error}");
    }
}
