//! Admission and execution control for externally submitted SPARQL.
//!
//! The Oxigraph API is synchronous and CPU/storage intensive. Running it
//! directly inside an Axum or MCP async handler pins a Tokio worker, while
//! allowing every request to enter simultaneously turns one expensive query
//! into a cell-wide OOM event. This module is the external boundary:
//!
//! - one process-wide semaphore bounds query + update concurrency;
//! - queries receive a server-clamped deadline and materialization ceiling;
//! - Oxigraph's real `CancellationToken` is fired on deadline or future drop;
//! - blocking evaluation runs on Tokio's blocking executor;
//! - a graph persistence lease spans seed/open/evaluate/collect, fencing graph
//!   deletion, restore/swap, and persistence teardown.
//!
//! Update execution deliberately has no advertised deadline yet. Oxigraph
//! 0.5.9 propagates its cancellation token through query-pattern updates, but
//! does not consult it while executing pure INSERT DATA / DELETE DATA / CLEAR /
//! LOAD operations. Returning "timed out" while one of those operations keeps
//! running and later commits would be false and dangerous. Updates still cross
//! the shared semaphore, blocking executor, and graph lifecycle lease here.

use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
    rdf_query_service::SparqlQueryResult,
    rdf_service::{
        run_sparql_query_service_controlled, run_sparql_update_service, MutationResult,
        SparqlInput, SparqlUpdateInput,
    },
};
use oxigraph::sparql::CancellationToken;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const CONCURRENCY_ENV: &str = "GARDEN_SPARQL_MAX_CONCURRENCY";
const DEFAULT_TIMEOUT_ENV: &str = "GARDEN_SPARQL_DEFAULT_TIMEOUT_MS";
const MAX_TIMEOUT_ENV: &str = "GARDEN_SPARQL_MAX_TIMEOUT_MS";

const DEFAULT_CONCURRENCY: usize = 2;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MIN_TIMEOUT_MS: u64 = 100;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExternalSparqlOptions {
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) max_rows: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffectiveQueryLimits {
    pub(crate) timeout: Duration,
    pub(crate) max_rows: usize,
}

pub(crate) struct SparqlAdmission {
    permits: Arc<Semaphore>,
    concurrency: usize,
    default_timeout: Duration,
    max_timeout: Duration,
}

impl SparqlAdmission {
    pub(crate) fn from_env() -> Self {
        let concurrency = positive_env_usize(CONCURRENCY_ENV).unwrap_or(DEFAULT_CONCURRENCY);
        let max_timeout_ms = positive_env_u64(MAX_TIMEOUT_ENV)
            .unwrap_or(MAX_TIMEOUT_MS)
            .max(MIN_TIMEOUT_MS);
        let default_timeout_ms = positive_env_u64(DEFAULT_TIMEOUT_ENV)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, max_timeout_ms);
        Self::new(
            concurrency,
            Duration::from_millis(default_timeout_ms),
            Duration::from_millis(max_timeout_ms),
        )
    }

    fn new(concurrency: usize, default_timeout: Duration, max_timeout: Duration) -> Self {
        let concurrency = concurrency.clamp(1, Semaphore::MAX_PERMITS);
        let max_timeout = max_timeout.max(Duration::from_millis(MIN_TIMEOUT_MS));
        let default_timeout = default_timeout
            .max(Duration::from_millis(MIN_TIMEOUT_MS))
            .min(max_timeout);
        Self {
            permits: Arc::new(Semaphore::new(concurrency)),
            concurrency,
            default_timeout,
            max_timeout,
        }
    }

    pub(crate) fn effective_query_limits(
        &self,
        requested: ExternalSparqlOptions,
    ) -> EffectiveQueryLimits {
        let timeout = requested
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout)
            .max(Duration::from_millis(MIN_TIMEOUT_MS))
            .min(self.max_timeout);
        let server_max_rows = crate::rdf_query_service::sparql_server_max_rows().max(1);
        let max_rows = requested
            .max_rows
            .unwrap_or(server_max_rows)
            .max(1)
            .min(server_max_rows);
        EffectiveQueryLimits { timeout, max_rows }
    }

    async fn acquire_before(
        &self,
        deadline: tokio::time::Instant,
    ) -> AppResult<OwnedSemaphorePermit> {
        match tokio::time::timeout_at(deadline, Arc::clone(&self.permits).acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(AppError::internal("SPARQL admission semaphore closed")),
            Err(_) => Err(AppError::capacity(format!(
                "SPARQL admission deadline elapsed while {} operation(s) occupied the executor",
                self.concurrency
            ))),
        }
    }

    async fn acquire_without_deadline(&self) -> AppResult<OwnedSemaphorePermit> {
        Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| AppError::internal("SPARQL admission semaphore closed"))
    }
}

/// Runs an external query without pinning the async runtime.
///
/// Deadline expiry cancels Oxigraph and then waits for the blocking worker to
/// observe cancellation before reporting 504. Dropping this future (for
/// example, because an HTTP client disconnected) also fires the token; the
/// blocking worker retains the semaphore + graph lease until it really exits.
pub(crate) async fn run_external_sparql_query(
    app: AppHandle,
    input: SparqlInput,
    requested: ExternalSparqlOptions,
) -> AppResult<SparqlQueryResult> {
    let admission = app
        .try_state::<SparqlAdmission>()
        .ok_or_else(|| AppError::internal("SPARQL admission state is not installed"))?;
    let limits = admission.effective_query_limits(requested);
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + limits.timeout;
    let permit = admission.acquire_before(deadline).await?;
    let graph_lease = acquire_graph_lease_before(&app, &input.graph_id, deadline).await?;
    let cancellation = CancellationToken::new();
    let mut cancel_on_drop = CancelOnDrop::new(cancellation.clone());
    let worker_cancellation = cancellation.clone();
    let worker_app = app.clone();

    let mut worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = run_sparql_query_service_controlled(
            worker_app,
            input,
            limits.max_rows,
            worker_cancellation,
        );
        if result.is_ok() {
            if let Some(graph_lease) = graph_lease.as_ref() {
                graph_lease.declare_rdf_writes_self_tracked();
            }
        }
        let _graph_lease = graph_lease;
        result
    });

    match tokio::time::timeout_at(deadline, &mut worker).await {
        Ok(joined) => {
            cancel_on_drop.disarm();
            joined.map_err(join_error)?
        }
        Err(_) => {
            cancellation.cancel();
            // Do not detach a still-running engine and call that a timeout.
            // The permit + graph lease remain inside the worker until actual
            // Oxigraph cancellation has unwound the iterator.
            let _ = worker.await.map_err(join_error)?;
            cancel_on_drop.disarm();
            Err(AppError::deadline(format!(
                "SPARQL query exceeded the server deadline of {} ms (elapsed {} ms) and was cancelled",
                limits.timeout.as_millis(),
                started.elapsed().as_millis()
            )))
        }
    }
}

/// Runs an external update on the blocking executor under bounded admission
/// and the graph lifecycle lease.
///
/// No deadline is accepted here until every Oxigraph update operation is
/// cooperatively cancellable. This function awaits the real commit/failure,
/// so a 200 response is truthful.
pub(crate) async fn run_external_sparql_update(
    app: AppHandle,
    input: SparqlUpdateInput,
) -> AppResult<MutationResult> {
    crate::restore_guard::require_no_active_restore(&app, &input.graph_id)
        .map_err(AppError::conflict)?;
    let admission = app
        .try_state::<SparqlAdmission>()
        .ok_or_else(|| AppError::internal("SPARQL admission state is not installed"))?;
    let permit = admission.acquire_without_deadline().await?;
    let graph_lease = acquire_graph_lease(&app, &input.graph_id).await?;
    // Close the race in which restore engages its guard while this update is
    // waiting for either admission or the graph lease. If this check wins,
    // the worker retains the graph lease and finishes before restore can
    // enter; if restore's guard wins, this update is rejected instead of
    // applying to freshly restored state.
    crate::restore_guard::require_no_active_restore(&app, &input.graph_id)
        .map_err(AppError::conflict)?;
    let worker_app = app.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _graph_lease = graph_lease;
        run_sparql_update_service(worker_app, input)
    })
    .await
    .map_err(join_error)?
}

async fn acquire_graph_lease_before(
    app: &AppHandle,
    graph_id: &str,
    deadline: tokio::time::Instant,
) -> AppResult<Option<crate::crdt_engine::persistence_coordinator::HotWriteLease>> {
    match tokio::time::timeout_at(deadline, acquire_graph_lease(app, graph_id)).await {
        Ok(result) => result,
        Err(_) => Err(AppError::capacity(format!(
            "SPARQL admission deadline elapsed waiting for graph lifecycle lease {graph_id}"
        ))),
    }
}

async fn acquire_graph_lease(
    app: &AppHandle,
    graph_id: &str,
) -> AppResult<Option<crate::crdt_engine::persistence_coordinator::HotWriteLease>> {
    match app.try_state::<GraphPersistenceCoordinator>() {
        Some(coordinator) => coordinator
            .acquire_hot_write(graph_id)
            .await
            .map(Some)
            .map_err(AppError::storage),
        None => Ok(None),
    }
}

fn join_error(error: tokio::task::JoinError) -> AppError {
    AppError::internal(format!("SPARQL blocking worker failed: {error}"))
}

fn positive_env_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn positive_env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
}

struct CancelOnDrop {
    cancellation: CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_limits_are_server_clamped() {
        let admission = SparqlAdmission::new(2, Duration::from_secs(5), Duration::from_secs(10));
        let server_max_rows = crate::rdf_query_service::sparql_server_max_rows();
        assert_eq!(
            SparqlAdmission::new(usize::MAX, Duration::from_secs(5), Duration::from_secs(10))
                .concurrency,
            Semaphore::MAX_PERMITS
        );

        assert_eq!(
            admission.effective_query_limits(ExternalSparqlOptions::default()),
            EffectiveQueryLimits {
                timeout: Duration::from_secs(5),
                max_rows: server_max_rows,
            }
        );
        assert_eq!(
            admission.effective_query_limits(ExternalSparqlOptions {
                timeout_ms: Some(99_000),
                max_rows: Some(server_max_rows.saturating_mul(2)),
            }),
            EffectiveQueryLimits {
                timeout: Duration::from_secs(10),
                max_rows: server_max_rows,
            }
        );
        assert_eq!(
            admission.effective_query_limits(ExternalSparqlOptions {
                timeout_ms: Some(0),
                max_rows: Some(0),
            }),
            EffectiveQueryLimits {
                timeout: Duration::from_millis(MIN_TIMEOUT_MS),
                max_rows: 1,
            }
        );
    }

    #[test]
    fn dropping_guard_fires_real_oxigraph_token() {
        let token = CancellationToken::new();
        {
            let _guard = CancelOnDrop::new(token.clone());
            assert!(!token.is_cancelled());
        }
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn shared_semaphore_bounds_queries_and_updates() {
        let admission = SparqlAdmission::new(1, Duration::from_secs(1), Duration::from_secs(1));
        let first = admission.acquire_without_deadline().await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(25);
        let blocked = admission.acquire_before(deadline).await;
        assert!(
            matches!(blocked, Err(error) if error.kind() == crate::app_error::AppErrorKind::Capacity)
        );
        drop(first);
        admission.acquire_without_deadline().await.unwrap();
    }
}
