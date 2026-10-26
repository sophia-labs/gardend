use crate::app_runtime::AppHandle;
use crate::{
    clock,
    graph_paths::existing_graph_dir,
    graph_record_store::read_graph_record_no_heal,
    graph_service::{list_graphs, GraphRecord},
    restore_guard::RestoreGuardState,
    time_travel_service::{capture_restore_point, delete_restore_point_response},
    time_travel_store::read_index,
    time_travel_types::RestorePointTrigger,
};
use std::{sync::Arc, time::Duration};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::time::MissedTickBehavior;

/// Tick cadence for unattended restore-point capture. Short in dev for
/// visibility while iterating, longer in release so we don't churn disk.
#[cfg(debug_assertions)]
const TICK_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(not(debug_assertions))]
const TICK_INTERVAL: Duration = Duration::from_secs(1800);

/// How many interval-trigger restore points to retain per graph. Manual and
/// checkpoint (auto-backup) restore points are never auto-pruned.
const INTERVAL_RETENTION_LIMIT: usize = 12;

pub(crate) fn start_interval_scheduler(app: AppHandle) {
    crate::app_runtime::async_runtime::spawn(async move {
        run_scheduler(app).await;
    });
}

async fn run_scheduler(app: AppHandle) {
    let lifecycle = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .inner()
        .clone();
    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    // Avoid bursty catch-up if the host was suspended; align to current time.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // First tick fires immediately — skip it so app boot doesn't capture.
    ticker.tick().await;
    log::info!(
        "time-travel interval scheduler armed at {}s cadence (retention={} per graph)",
        TICK_INTERVAL.as_secs(),
        INTERVAL_RETENTION_LIMIT
    );
    loop {
        ticker.tick().await;
        let _maintenance = match lifecycle.begin_maintenance("interval-restore-point") {
            Ok(lease) => lease,
            Err(_) => {
                log::info!("time-travel interval scheduler stopped for cell drain");
                return;
            }
        };
        let tick_app = app.clone();
        match crate::app_runtime::async_runtime::spawn_blocking(move || tick_once(&tick_app)).await
        {
            Ok(Ok(summary)) => {
                if summary.captured > 0 || summary.skipped_unchanged > 0 {
                    log::debug!(
                        "interval restore-point tick complete: captured={}, skipped_unchanged={}",
                        summary.captured,
                        summary.skipped_unchanged
                    );
                }
            }
            Ok(Err(error)) => {
                log::warn!("interval restore-point tick failed: {error}");
            }
            Err(error) => {
                log::warn!("interval restore-point tick task failed: {error}");
            }
        }
    }
}

#[derive(Debug, Default)]
struct TickSummary {
    captured: usize,
    skipped_unchanged: usize,
}

fn tick_once(app: &AppHandle) -> Result<TickSummary, String> {
    let guard = app.state::<std::sync::Arc<RestoreGuardState>>();
    if guard.is_active() {
        log::debug!("interval restore-point capture skipped — restore in progress");
        return Ok(TickSummary::default());
    }
    let graphs = scheduled_graphs(app)?;
    let tick_span = tracing::info_span!(
        target: "gardend_otel",
        "time_travel.interval.tick",
        sophia.tick.graphs = graphs.len() as u64,
        sophia.tick.captured = tracing::field::Empty,
        sophia.tick.skipped_unchanged = tracing::field::Empty,
    );
    let _tick_entered = tick_span.enter();
    let mut summary = TickSummary::default();
    for graph in graphs {
        match graph_needs_interval_capture(app, &graph) {
            Ok(true) => {}
            Ok(false) => {
                summary.skipped_unchanged += 1;
                continue;
            }
            Err(error) => {
                log::warn!(
                    "interval capture eligibility failed for graph {}: {error}",
                    graph.graph_id
                );
                continue;
            }
        }
        if let Err(error) = capture_for_graph(app, &graph.graph_id) {
            log::warn!(
                "interval capture failed for graph {}: {error}",
                graph.graph_id
            );
        } else {
            summary.captured += 1;
        }
    }
    tick_span.record("sophia.tick.captured", summary.captured as u64);
    tick_span.record(
        "sophia.tick.skipped_unchanged",
        summary.skipped_unchanged as u64,
    );
    Ok(summary)
}

fn scheduled_graphs(app: &AppHandle) -> Result<Vec<GraphRecord>, String> {
    if let Some(boundary) = app.try_state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>() {
        if let Some(owner_graph_id) = boundary.owner_graph_id() {
            // Never enumerate a poisoned/shared profile from a single-graph
            // cell. Read exactly the configured owner without self-healing;
            // the separate self-heal/default-off policy remains untouched.
            return read_graph_record_no_heal(app, owner_graph_id)
                .map(|(_, graph)| vec![graph])
                .map_err(crate::app_error::AppError::message);
        }
    }
    list_graphs(app.clone())
}

fn graph_needs_interval_capture(app: &AppHandle, graph: &GraphRecord) -> Result<bool, String> {
    let graph_dir = existing_graph_dir(app, &graph.graph_id)?;
    if !graph_has_documents(&graph_dir) {
        return Ok(false);
    }
    let graph_updated_at = match graph.updated_at.parse::<i64>() {
        Ok(value) => value,
        Err(_) => return Ok(true),
    };
    let index = read_index(&graph_dir, &graph.graph_id)?;
    let latest_interval = index
        .entries
        .iter()
        .filter(|entry| matches!(entry.trigger, RestorePointTrigger::Interval))
        .map(|entry| entry.created_at)
        .max();
    Ok(latest_interval.is_none_or(|created_at| created_at < graph_updated_at))
}

fn capture_for_graph(app: &AppHandle, graph_id: &str) -> Result<(), String> {
    let label = Some(format!("interval-{}", clock::timestamp()));
    let _ = capture_restore_point(app, graph_id, RestorePointTrigger::Interval, label)?;
    enforce_interval_retention(app, graph_id)?;
    Ok(())
}

fn graph_has_documents(graph_dir: &std::path::Path) -> bool {
    let documents_dir = graph_dir.join("documents");
    if !documents_dir.is_dir() {
        return false;
    }
    std::fs::read_dir(&documents_dir)
        .ok()
        .map(|mut entries| {
            entries.any(|entry| {
                entry
                    .ok()
                    .and_then(|e| Some(e.path().join("document.json").is_file()))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn enforce_interval_retention(app: &AppHandle, graph_id: &str) -> Result<(), String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let index = read_index(&graph_dir, graph_id)?;
    let mut interval_entries: Vec<_> = index
        .entries
        .into_iter()
        .filter(|entry| matches!(entry.trigger, RestorePointTrigger::Interval))
        .collect();
    // Newest first — retain head, evict the tail past the limit.
    interval_entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    if interval_entries.len() <= INTERVAL_RETENTION_LIMIT {
        return Ok(());
    }
    for stale in interval_entries.into_iter().skip(INTERVAL_RETENTION_LIMIT) {
        if let Err(error) = delete_restore_point_response(app, graph_id, &stale.restore_point_id) {
            log::warn!(
                "interval retention: failed to evict {}: {error}",
                stale.restore_point_id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use serde_json::json;

    fn write_document(app: &AppHandle, graph_id: &str, document_id: &str, text: &str) {
        crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.write".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(document_id.to_string()),
                payload: json!({
                    "documentId": document_id,
                    "content": text,
                    "format": "markdown",
                    "title": document_id,
                }),
            },
        ))
        .expect("document.write drains through the real engine");
    }

    /// The 30-minute tick: a graph with nothing new since its last interval point is skipped without
    /// reading a document, a changed graph gets one point that reuses what did not change, and the point is
    /// taken without parking the graph's hot-write gate afterwards (a writer proceeds at once).
    #[test]
    fn tick_skips_quiet_graphs_and_captures_changed_ones() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-tt-tick-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "tt-tick";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Tick".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            write_document(&app, graph_id, "tick-a", "alpha");
            write_document(&app, graph_id, "tick-b", "beta");
            // Let the writes' asynchronous tails land so they cannot bump the graph after the first point.
            std::thread::sleep(Duration::from_millis(200));
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let interval_points = || {
                read_index(&graph_dir, graph_id)
                    .expect("index")
                    .entries
                    .into_iter()
                    .filter(|entry| matches!(entry.trigger, RestorePointTrigger::Interval))
                    .count()
            };

            let first = tick_once(&app).expect("first tick");
            assert_eq!((first.captured, first.skipped_unchanged), (1, 0));
            assert_eq!(interval_points(), 1);

            std::thread::sleep(Duration::from_millis(50));
            let quiet = tick_once(&app).expect("quiet tick");
            assert_eq!(
                (quiet.captured, quiet.skipped_unchanged),
                (0, 1),
                "nothing changed since the last interval point"
            );
            assert_eq!(interval_points(), 1);

            std::thread::sleep(Duration::from_millis(5));
            write_document(&app, graph_id, "tick-a", "alpha, changed");
            std::thread::sleep(Duration::from_millis(200));
            let changed = tick_once(&app).expect("changed tick");
            assert_eq!((changed.captured, changed.skipped_unchanged), (1, 0));
            assert_eq!(interval_points(), 2);

            // The untouched document is one snapshot in both points; the changed one is not.
            let mut entries = read_index(&graph_dir, graph_id).expect("index").entries;
            entries.sort_by_key(|entry| entry.created_at);
            let older =
                crate::time_travel_store::read_manifest(&graph_dir, &entries[0].restore_point_id)
                    .expect("older manifest");
            let newer =
                crate::time_travel_store::read_manifest(&graph_dir, &entries[1].restore_point_id)
                    .expect("newer manifest");
            let snapshot_of = |manifest: &crate::time_travel_types::RestorePointManifest,
                               id: &str| {
                manifest
                    .documents
                    .iter()
                    .find(|doc| doc.document_id == id)
                    .map(|doc| doc.snapshot_id.clone())
                    .expect("document in point")
            };
            assert_eq!(snapshot_of(&older, "tick-b"), snapshot_of(&newer, "tick-b"));
            assert_ne!(snapshot_of(&older, "tick-a"), snapshot_of(&newer, "tick-a"));

            // The gate is free once the tick is done: a hot-write lease is granted without waiting.
            let lease =
                crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
                    &app, graph_id,
                )
                .expect("hot-write lease is free after the tick");
            drop(lease);
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
