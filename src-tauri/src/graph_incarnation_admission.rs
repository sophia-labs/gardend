//! Optional caller fences carried into the existing graph lifecycle authority.
use crate::app_error::{AppError, AppResult};
use crate::app_runtime::AppHandle;
use crate::crdt_engine::persistence_coordinator::{GraphPersistenceCoordinator, SharedLease};
use serde_json::Value;
#[cfg(feature = "desktop")]
use tauri::Manager;

pub(crate) fn expected_incarnation(arguments: &Value) -> AppResult<Option<String>> {
    let mut expected: Option<String> = None;
    for key in ["graphIncarnation", "graph_incarnation"] {
        if let Some(value) = arguments.get(key) {
            let text = value
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256 && s.trim() == *s)
                .ok_or_else(|| {
                    AppError::validation(format!(
                        "{key} must be a non-empty unpadded string of at most 256 bytes"
                    ))
                })?;
            if expected.as_deref().is_some_and(|other| other != text) {
                return Err(AppError::validation("graph incarnation aliases disagree"));
            }
            expected = Some(text.to_owned());
        }
    }
    Ok(expected)
}

/// Caller holds the graph's managed lifecycle lease. This never heals a graph
/// or backfills a missing token before refusing a supplied expectation.
pub(crate) fn require_current(app: &AppHandle, graph_id: &str, expected: &str) -> AppResult<()> {
    let (_, record) = crate::graph_record_store::read_graph_record_no_heal(app, graph_id)?;
    if record.incarnation_id.as_deref() != Some(expected) {
        return Err(AppError::conflict(format!(
            "stale graph incarnation for {graph_id}: expected {expected}, actual {}",
            record.incarnation_id.as_deref().unwrap_or("missing"),
        ))
        .with_code(crate::app_error_codes::STALE_GRAPH_INCARNATION));
    }
    Ok(())
}

pub(crate) async fn acquire_expected_lifetime(
    app: &AppHandle,
    graph_id: &str,
    expected: Option<&str>,
) -> AppResult<Option<SharedLease>> {
    let Some(expected) = expected else {
        return Ok(None);
    };
    let coordinator = app
        .try_state::<GraphPersistenceCoordinator>()
        .ok_or_else(|| {
            AppError::validation(
                "expected-incarnation mutation requires a managed graph lifecycle coordinator",
            )
        })?;
    let lease = coordinator
        .acquire_lifecycle_shared(graph_id)
        .await
        .map_err(AppError::storage)?;
    require_current(app, graph_id, expected)?;
    Ok(Some(lease))
}
