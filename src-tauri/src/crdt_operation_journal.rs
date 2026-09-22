use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    crdt_operation_types::{CompleteCrdtOperationInput, CrdtOperation},
    profile_paths::profile_dir,
    storage::{append_secret_line, display_path},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const CRDT_OPERATION_JOURNAL_SCHEMA_VERSION: u32 = 1;
const CRDT_OPERATION_JOURNAL_DIR: &str = "worklog";
const CRDT_OPERATION_JOURNAL_FILE: &str = "crdt-operations.jsonl";
// 7-day window matches ledger GC policy; see docs/a2-ledger-design.md "Decisions §1".
pub(crate) const LOCAL_CRDT_JOURNAL_RETENTION_DAYS: u64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum CrdtOperationJournalStatus {
    Queued,
    Succeeded,
    Failed,
    CallerTimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CrdtOperationJournalEvent {
    schema_version: u32,
    event_id: String,
    timestamp: String,
    operation_id: String,
    status: CrdtOperationJournalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation: Option<CrdtOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(crate) fn crdt_operation_journal_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?
        .join(CRDT_OPERATION_JOURNAL_DIR)
        .join(CRDT_OPERATION_JOURNAL_FILE))
}

pub(crate) fn record_crdt_operation_queued(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> Result<(), String> {
    append_journal_event(
        &crdt_operation_journal_path(app)?,
        CrdtOperationJournalEvent {
            schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
            event_id: journal_event_id(),
            timestamp: timestamp(),
            operation_id: operation.operation_id.clone(),
            status: CrdtOperationJournalStatus::Queued,
            operation: Some(operation.clone()),
            error: None,
        },
    )
}

pub(crate) fn record_crdt_operation_completion(
    app: &AppHandle,
    input: &CompleteCrdtOperationInput,
) -> Result<(), String> {
    append_journal_event(
        &crdt_operation_journal_path(app)?,
        CrdtOperationJournalEvent {
            schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
            event_id: journal_event_id(),
            timestamp: timestamp(),
            operation_id: input.operation_id.clone(),
            status: if input.ok {
                CrdtOperationJournalStatus::Succeeded
            } else {
                CrdtOperationJournalStatus::Failed
            },
            operation: None,
            error: input.error.clone(),
        },
    )
}

pub(crate) fn record_crdt_operation_caller_timeout(
    app: &AppHandle,
    operation_id: &str,
) -> Result<(), String> {
    append_journal_event(
        &crdt_operation_journal_path(app)?,
        CrdtOperationJournalEvent {
            schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
            event_id: journal_event_id(),
            timestamp: timestamp(),
            operation_id: operation_id.to_string(),
            status: CrdtOperationJournalStatus::CallerTimedOut,
            operation: None,
            error: Some("caller timed out waiting for desktop runtime".to_string()),
        },
    )
}

pub(crate) fn recover_pending_crdt_operations(
    app: &AppHandle,
) -> Result<Vec<CrdtOperation>, String> {
    pending_crdt_operations_from_journal(&crdt_operation_journal_path(app)?)
}

fn journal_event_id() -> String {
    format!("evt-{}", Uuid::new_v4().simple())
}

fn append_journal_event(path: &Path, event: CrdtOperationJournalEvent) -> Result<(), String> {
    let _durability_guard = crate::cell_durability::write_guard();
    let line = serde_json::to_string(&event)
        .map_err(|error| format!("serialize CRDT operation journal event: {error}"))?;
    let _jsonl_guard = crate::jsonl_mutation_lock::acquire_jsonl_mutation_lock(path)?;
    append_secret_line(path, &line).map_err(Into::into)
}

fn pending_crdt_operations_from_journal(path: &Path) -> Result<Vec<CrdtOperation>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", display_path(path))),
    };
    let mut pending = BTreeMap::<String, (usize, CrdtOperation)>::new();
    for (line_index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<CrdtOperationJournalEvent>(line) {
            Ok(event) => event,
            Err(error) => {
                log::warn!(
                    "Skipping unreadable CRDT operation journal line {} in {}: {}",
                    line_index + 1,
                    display_path(path),
                    error
                );
                continue;
            }
        };
        if event.schema_version != CRDT_OPERATION_JOURNAL_SCHEMA_VERSION {
            log::warn!(
                "Skipping unsupported CRDT operation journal schema {} in {}",
                event.schema_version,
                display_path(path)
            );
            continue;
        }
        match event.status {
            CrdtOperationJournalStatus::Queued => {
                if let Some(mut operation) = event.operation {
                    operation.enqueue_timestamp = event.timestamp;
                    pending.insert(event.operation_id, (line_index, operation));
                }
            }
            CrdtOperationJournalStatus::CallerTimedOut => {}
            CrdtOperationJournalStatus::Succeeded | CrdtOperationJournalStatus::Failed => {
                pending.remove(&event.operation_id);
            }
        }
    }
    let mut pending = pending.into_values().collect::<Vec<_>>();
    pending.sort_by_key(|(line_index, _)| *line_index);
    Ok(pending
        .into_iter()
        .map(|(_, operation)| operation)
        .collect())
}

pub(crate) fn prune_crdt_operation_journal(
    app: &AppHandle,
    retention_days: u64,
) -> Result<usize, String> {
    let path = crdt_operation_journal_path(app)?;
    prune_journal_at_path(&path, retention_days)
}

fn prune_journal_at_path(path: &Path, retention_days: u64) -> Result<usize, String> {
    let _durability_guard = crate::cell_durability::write_guard();
    let _jsonl_guard = crate::jsonl_mutation_lock::acquire_jsonl_mutation_lock(path)?;
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("read {}: {error}", display_path(path))),
    };

    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let retention_millis = retention_days * 86_400 * 1_000;

    // Parse all lines, retaining the raw text alongside each parsed event.
    let lines_with_events: Vec<(String, Option<CrdtOperationJournalEvent>)> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let event = serde_json::from_str::<CrdtOperationJournalEvent>(line).ok();
            (line.to_string(), event)
        })
        .collect();

    // For each operationId, track whether it is terminal and the timestamp of
    // its latest terminal event.  None means non-terminal (keep regardless of age).
    let mut terminal_latest: HashMap<String, Option<u64>> = HashMap::new();
    for (_, maybe_event) in &lines_with_events {
        let Some(event) = maybe_event else {
            continue;
        };
        let entry = terminal_latest
            .entry(event.operation_id.clone())
            .or_insert(None);
        match event.status {
            CrdtOperationJournalStatus::Succeeded | CrdtOperationJournalStatus::Failed => {
                let ts = event.timestamp.parse::<u64>().unwrap_or(0);
                *entry = Some(entry.map_or(ts, |prev: u64| prev.max(ts)));
            }
            CrdtOperationJournalStatus::Queued | CrdtOperationJournalStatus::CallerTimedOut => {}
        }
    }

    // Collect operation IDs whose terminal event is older than the retention window.
    let drop_ids: std::collections::HashSet<String> = terminal_latest
        .into_iter()
        .filter_map(|(id, latest_ts)| {
            let ts = latest_ts?;
            if now_millis.saturating_sub(ts) > retention_millis {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    if drop_ids.is_empty() {
        return Ok(0);
    }

    // Write kept lines to a sibling temp file, then atomically rename over the original.
    // std::fs::rename is atomic on Unix (same filesystem); see POSIX rename(2).
    let temp_path = path.with_extension("jsonl.tmp");
    let mut removed = 0usize;
    {
        let parent = path
            .parent()
            .ok_or_else(|| format!("journal path has no parent: {}", display_path(path)))?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("create dir {}: {e}", display_path(parent)))?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(|e| format!("open temp {}: {e}", display_path(&temp_path)))?;
        for (raw_line, maybe_event) in &lines_with_events {
            let keep = match maybe_event {
                Some(event) => !drop_ids.contains(&event.operation_id),
                // Unparseable lines are preserved to avoid accidental data loss.
                None => true,
            };
            if keep {
                writeln!(file, "{raw_line}")
                    .map_err(|e| format!("write temp {}: {e}", display_path(&temp_path)))?;
            } else {
                removed += 1;
            }
        }
        file.sync_all()
            .map_err(|e| format!("sync temp {}: {e}", display_path(&temp_path)))?;
    }

    fs::rename(&temp_path, path).map_err(|e| {
        format!(
            "rename {} -> {}: {e}",
            display_path(&temp_path),
            display_path(path)
        )
    })?;

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{mpsc, Arc, Barrier},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    fn temp_journal_path(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("mnemosyne-crdt-journal-{name}-{suffix}"))
            .join(CRDT_OPERATION_JOURNAL_FILE)
    }

    fn operation(id: &str) -> CrdtOperation {
        CrdtOperation {
            operation_id: id.to_string(),
            kind: "document.write".to_string(),
            graph_id: "graph-1".to_string(),
            document_id: Some("doc-1".to_string()),
            payload: serde_json::json!({ "content": "hello" }),
            enqueue_timestamp: "0".to_string(),
        }
    }

    #[test]
    fn journal_recovers_only_unfinished_operations() {
        let path = temp_journal_path("pending");
        let first = operation("op-1");
        let second = operation("op-2");
        append_journal_event(
            &path,
            CrdtOperationJournalEvent {
                schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                event_id: "evt-1".to_string(),
                timestamp: "1".to_string(),
                operation_id: first.operation_id.clone(),
                status: CrdtOperationJournalStatus::Queued,
                operation: Some(first.clone()),
                error: None,
            },
        )
        .expect("append first queued event");
        append_journal_event(
            &path,
            CrdtOperationJournalEvent {
                schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                event_id: "evt-2".to_string(),
                timestamp: "2".to_string(),
                operation_id: second.operation_id.clone(),
                status: CrdtOperationJournalStatus::Queued,
                operation: Some(second.clone()),
                error: None,
            },
        )
        .expect("append second queued event");
        append_journal_event(
            &path,
            CrdtOperationJournalEvent {
                schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                event_id: "evt-3".to_string(),
                timestamp: "3".to_string(),
                operation_id: first.operation_id.clone(),
                status: CrdtOperationJournalStatus::Succeeded,
                operation: None,
                error: None,
            },
        )
        .expect("append first completion");

        let pending = pending_crdt_operations_from_journal(&path).expect("read pending");

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation_id, second.operation_id);
        let _ = fs::remove_dir_all(path.parent().expect("journal parent"));
    }

    #[test]
    fn caller_timeout_keeps_operation_recoverable() {
        let path = temp_journal_path("timeout");
        let queued = operation("op-timeout");
        append_journal_event(
            &path,
            CrdtOperationJournalEvent {
                schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                event_id: "evt-1".to_string(),
                timestamp: "1".to_string(),
                operation_id: queued.operation_id.clone(),
                status: CrdtOperationJournalStatus::Queued,
                operation: Some(queued.clone()),
                error: None,
            },
        )
        .expect("append queued event");
        append_journal_event(
            &path,
            CrdtOperationJournalEvent {
                schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                event_id: "evt-2".to_string(),
                timestamp: "2".to_string(),
                operation_id: queued.operation_id.clone(),
                status: CrdtOperationJournalStatus::CallerTimedOut,
                operation: None,
                error: Some("timeout".to_string()),
            },
        )
        .expect("append timeout event");

        let pending = pending_crdt_operations_from_journal(&path).expect("read pending");

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation_id, queued.operation_id);
        let _ = fs::remove_dir_all(path.parent().expect("journal parent"));
    }

    fn old_ts() -> String {
        // 8 days ago in milliseconds — clearly outside the 7-day retention window.
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        (millis - 8 * 86_400 * 1_000).to_string()
    }

    fn recent_ts() -> String {
        // 1 day ago — within the 7-day retention window.
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        (millis - 86_400 * 1_000).to_string()
    }

    fn write_queued(path: &Path, op: &CrdtOperation, event_id: &str, ts: &str) {
        append_journal_event(
            path,
            CrdtOperationJournalEvent {
                schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                event_id: event_id.to_string(),
                timestamp: ts.to_string(),
                operation_id: op.operation_id.clone(),
                status: CrdtOperationJournalStatus::Queued,
                operation: Some(op.clone()),
                error: None,
            },
        )
        .expect("write queued event");
    }

    fn write_succeeded(path: &Path, operation_id: &str, event_id: &str, ts: &str) {
        append_journal_event(
            path,
            CrdtOperationJournalEvent {
                schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                event_id: event_id.to_string(),
                timestamp: ts.to_string(),
                operation_id: operation_id.to_string(),
                status: CrdtOperationJournalStatus::Succeeded,
                operation: None,
                error: None,
            },
        )
        .expect("write succeeded event");
    }

    #[test]
    fn prune_drops_terminal_operations_older_than_retention() {
        let path = temp_journal_path("prune-drops");

        let old_terminal = operation("op-old-terminal");
        let recent_terminal = operation("op-recent-terminal");
        let old_nonterminal = operation("op-old-nonterminal");

        let old = old_ts();
        let recent = recent_ts();

        // old_terminal: Queued + Succeeded both old — should be dropped.
        write_queued(&path, &old_terminal, "e1", &old);
        write_succeeded(&path, &old_terminal.operation_id, "e2", &old);

        // recent_terminal: Queued + Succeeded recent — should be kept.
        write_queued(&path, &recent_terminal, "e3", &recent);
        write_succeeded(&path, &recent_terminal.operation_id, "e4", &recent);

        // old_nonterminal: only Queued, old — non-terminal, must be kept regardless of age.
        write_queued(&path, &old_nonterminal, "e5", &old);

        let removed =
            prune_journal_at_path(&path, LOCAL_CRDT_JOURNAL_RETENTION_DAYS).expect("prune");

        // Two events for old_terminal should be dropped; others kept.
        assert_eq!(removed, 2, "expected 2 events removed for old terminal op");

        let raw = fs::read_to_string(&path).expect("read after prune");
        assert!(
            !raw.contains("op-old-terminal"),
            "old terminal op must not appear after prune"
        );
        assert!(
            raw.contains("op-recent-terminal"),
            "recent terminal op must be preserved"
        );
        assert!(
            raw.contains("op-old-nonterminal"),
            "non-terminal op must be preserved regardless of age"
        );

        let _ = fs::remove_dir_all(path.parent().expect("journal parent"));
    }

    #[test]
    fn prune_handles_empty_journal() {
        let path = temp_journal_path("prune-empty");
        // File does not exist — prune should return Ok(0) without error.
        let removed =
            prune_journal_at_path(&path, LOCAL_CRDT_JOURNAL_RETENTION_DAYS).expect("prune missing");
        assert_eq!(removed, 0);

        // Also works on an existing but empty file.
        fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        fs::write(&path, "").expect("write empty file");
        let removed =
            prune_journal_at_path(&path, LOCAL_CRDT_JOURNAL_RETENTION_DAYS).expect("prune empty");
        assert_eq!(removed, 0);

        let _ = fs::remove_dir_all(path.parent().expect("journal parent"));
    }

    #[test]
    fn prune_preserves_caller_timed_out_operations() {
        let path = temp_journal_path("prune-timeout");

        let op = operation("op-timedout");
        let old = old_ts();

        // Queued + CallerTimedOut — non-terminal, must survive pruning.
        write_queued(&path, &op, "e1", &old);
        append_journal_event(
            &path,
            CrdtOperationJournalEvent {
                schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                event_id: "e2".to_string(),
                timestamp: old.clone(),
                operation_id: op.operation_id.clone(),
                status: CrdtOperationJournalStatus::CallerTimedOut,
                operation: None,
                error: Some("timeout".to_string()),
            },
        )
        .expect("write caller-timed-out event");

        let removed =
            prune_journal_at_path(&path, LOCAL_CRDT_JOURNAL_RETENTION_DAYS).expect("prune");
        assert_eq!(removed, 0, "CallerTimedOut op must not be pruned");

        let raw = fs::read_to_string(&path).expect("read after prune");
        assert!(
            raw.contains("op-timedout"),
            "CallerTimedOut op events must still be present"
        );

        let _ = fs::remove_dir_all(path.parent().expect("journal parent"));
    }

    #[test]
    fn concurrent_appends_remain_distinct_parseable_records() {
        let path = temp_journal_path("concurrent-appends");
        let writers = 24usize;
        let barrier = Arc::new(Barrier::new(writers));
        let mut handles = Vec::new();
        for index in 0..writers {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let operation = operation(&format!("op-concurrent-{index}"));
                append_journal_event(
                    &path,
                    CrdtOperationJournalEvent {
                        schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                        event_id: format!("event-concurrent-{index}"),
                        timestamp: index.to_string(),
                        operation_id: operation.operation_id.clone(),
                        status: CrdtOperationJournalStatus::Queued,
                        operation: Some(operation),
                        error: None,
                    },
                )
            }));
        }
        for handle in handles {
            handle.join().expect("append thread").expect("append event");
        }

        let raw = fs::read_to_string(&path).expect("read concurrent journal");
        let lines = raw.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), writers);
        assert!(lines
            .iter()
            .all(|line| { serde_json::from_str::<CrdtOperationJournalEvent>(line).is_ok() }));
        assert_eq!(
            pending_crdt_operations_from_journal(&path)
                .expect("recover concurrent journal")
                .len(),
            writers
        );
        let _ = fs::remove_dir_all(path.parent().expect("journal parent"));
    }

    #[test]
    fn append_waits_for_prune_rewrite_and_survives_rename() {
        let path = temp_journal_path("prune-vs-append");
        let old_terminal = operation("op-old-prune-race");
        let old = old_ts();
        write_queued(&path, &old_terminal, "old-queued", &old);
        write_succeeded(&path, &old_terminal.operation_id, "old-done", &old);

        let (reached_tx, reached_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        crate::jsonl_mutation_lock::install_jsonl_lock_hold_hook_for_test(
            &path, reached_tx, resume_rx,
        )
        .expect("install held prune lock");

        let prune_path = path.clone();
        let prune = std::thread::spawn(move || {
            prune_journal_at_path(&prune_path, LOCAL_CRDT_JOURNAL_RETENTION_DAYS)
        });
        let expected_lock =
            crate::jsonl_mutation_lock::jsonl_mutation_lock_path(&path).expect("JSONL lock path");
        assert_eq!(
            reached_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("prune acquired JSONL lock"),
            expected_lock
        );

        let appended = operation("op-appended-during-prune");
        let append_path = path.clone();
        let append_operation = appended.clone();
        let (append_done_tx, append_done_rx) = mpsc::channel();
        let append = std::thread::spawn(move || {
            let result = append_journal_event(
                &append_path,
                CrdtOperationJournalEvent {
                    schema_version: CRDT_OPERATION_JOURNAL_SCHEMA_VERSION,
                    event_id: "event-appended-during-prune".to_string(),
                    timestamp: recent_ts(),
                    operation_id: append_operation.operation_id.clone(),
                    status: CrdtOperationJournalStatus::Queued,
                    operation: Some(append_operation),
                    error: None,
                },
            );
            let _ = append_done_tx.send(());
            result
        });
        assert!(
            matches!(
                append_done_rx.recv_timeout(Duration::from_millis(150)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "append must block behind the prune lock"
        );
        resume_tx.send(()).expect("resume prune");
        assert_eq!(prune.join().expect("prune thread").expect("prune"), 2);
        append.join().expect("append thread").expect("append");

        let pending = pending_crdt_operations_from_journal(&path).expect("recover after race");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation_id, appended.operation_id);
        assert!(!fs::read_to_string(&path)
            .expect("read pruned journal")
            .contains("op-old-prune-race"));
        crate::jsonl_mutation_lock::clear_jsonl_lock_hold_hook_for_test(&path);
        let _ = fs::remove_dir_all(path.parent().expect("journal parent"));
    }
}
