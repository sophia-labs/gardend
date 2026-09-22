use crate::app_runtime::AppHandle;
use crate::{
    profile_paths::profile_dir,
    storage::{append_secret_line, display_path},
};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

// Tier B operation-completion ledger: a Rust-side JSONL companion to the CRDT
// operation journal that records terminal completions for multi-step ops
// (graph.importArchive, import.vault, document.ingestMarkdownOriginal,
// document.uploadIngest). At handler entry, multi-step handlers consult this
// ledger to short-circuit a successful prior apply and return the cached
// envelope without re-touching the filesystem.
//
// See `docs/a2-ledger-design.md` for the design that motivates this module.
// The Tier A per-document Y.Map ledger (used by block.editText / block.insert)
// is in `frontend/src/crdt/operation-ledger.ts`.

const OPERATION_COMPLETION_LEDGER_SCHEMA_VERSION: u32 = 1;
const OPERATION_COMPLETION_LEDGER_DIR: &str = "worklog";
const OPERATION_COMPLETION_LEDGER_FILE: &str = "operation-completion.jsonl";
// 7-day window matches the CRDT journal's retention; see docs/a2-ledger-design.md "Decisions §1".
pub(crate) const LOCAL_COMPLETION_LEDGER_RETENTION_DAYS: u64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OperationCompletionEntry {
    pub(crate) schema_version: u32,
    pub(crate) operation_id: String,
    pub(crate) kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) graph_id: Option<String>,
    pub(crate) completed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) payload_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<serde_json::Value>,
}

#[cfg(test)]
impl OperationCompletionEntry {
    pub(crate) fn new(
        operation_id: impl Into<String>,
        kind: impl Into<String>,
        graph_id: Option<String>,
        completed_at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: OPERATION_COMPLETION_LEDGER_SCHEMA_VERSION,
            operation_id: operation_id.into(),
            kind: kind.into(),
            graph_id,
            completed_at: completed_at.into(),
            payload_hash: None,
            result: None,
        }
    }

    pub(crate) fn with_payload_hash(mut self, hash: impl Into<String>) -> Self {
        self.payload_hash = Some(hash.into());
        self
    }

    pub(crate) fn with_result(mut self, result: serde_json::Value) -> Self {
        self.result = Some(result);
        self
    }
}

pub(crate) fn operation_completion_ledger_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?
        .join(OPERATION_COMPLETION_LEDGER_DIR)
        .join(OPERATION_COMPLETION_LEDGER_FILE))
}

/// Append a completion entry to the Tier B ledger. Call this AFTER all steps of
/// a multi-step operation have succeeded; recovery interprets a missing entry
/// as "the operation was not fully completed and may need replay."
pub(crate) fn append_completion_entry(
    app: &AppHandle,
    entry: OperationCompletionEntry,
) -> Result<(), String> {
    #[cfg(test)]
    maybe_fail_completion_append_for_test(&entry.operation_id)?;
    let _flush_guard = crate::cell_durability::write_guard();
    let path = operation_completion_ledger_path(app)?;
    append_completion_entry_at_path(&path, entry)
}

#[cfg(test)]
static FAIL_NEXT_COMPLETION_APPEND: OnceLock<Mutex<Option<String>>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_completion_append_for_test(operation_id: impl Into<String>) {
    *FAIL_NEXT_COMPLETION_APPEND
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(operation_id.into());
}

#[cfg(test)]
fn maybe_fail_completion_append_for_test(operation_id: &str) -> Result<(), String> {
    let mut pending = FAIL_NEXT_COMPLETION_APPEND
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending.as_deref() == Some(operation_id) {
        pending.take();
        return Err(format!(
            "injected completion ledger append failure for {operation_id}"
        ));
    }
    Ok(())
}

fn append_completion_entry_at_path(
    path: &Path,
    entry: OperationCompletionEntry,
) -> Result<(), String> {
    let line =
        serde_json::to_string(&entry).map_err(|e| format!("serialize completion entry: {e}"))?;
    let _jsonl_guard = crate::jsonl_mutation_lock::acquire_jsonl_mutation_lock(path)?;
    append_secret_line(path, &line).map_err(|e| e.message())
}

/// Look up a completion entry by operationId. Returns Ok(None) if the ledger
/// does not contain the operation, or if the file does not exist.
///
/// Reads the file each call. For C-class multi-step ops this is acceptable —
/// they are infrequent and the file is small after pruning. If profiling shows
/// the read cost matters, an in-memory cache can be introduced later.
pub(crate) fn completion_entry_for(
    app: &AppHandle,
    operation_id: &str,
) -> Result<Option<OperationCompletionEntry>, String> {
    let path = operation_completion_ledger_path(app)?;
    completion_entry_for_at_path(&path, operation_id)
}

fn completion_entry_for_at_path(
    path: &Path,
    operation_id: &str,
) -> Result<Option<OperationCompletionEntry>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", display_path(path))),
    };
    // Iterate from the end so we return the most-recent entry if the same
    // operationId appears more than once (defensive — should not happen in
    // normal operation since handlers append once after success).
    for line in raw.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<OperationCompletionEntry>(line) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.schema_version != OPERATION_COMPLETION_LEDGER_SCHEMA_VERSION {
            // Permissive: unknown schema versions are still treated as "completed"
            // for membership-only purposes, but we cannot return the entry as the
            // caller expects (no usable result). Return None here so the caller
            // re-runs; the alternative (returning a partial entry) would require
            // a separate "opaque hit" type. Multi-step ops re-running is safe so
            // long as per-step idempotency holds.
            continue;
        }
        if entry.operation_id == operation_id {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Prune entries older than `retention_days`. Mirrors the CRDT journal prune at
/// `crdt_operation_journal::prune_crdt_operation_journal`. Stream-rewrites the
/// JSONL to a sibling temp file, then atomically renames over the original.
pub(crate) fn prune_completion_ledger(
    app: &AppHandle,
    retention_days: u64,
) -> Result<usize, String> {
    let path = operation_completion_ledger_path(app)?;
    prune_completion_ledger_at_path(&path, retention_days)
}

fn prune_completion_ledger_at_path(path: &Path, retention_days: u64) -> Result<usize, String> {
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

    let lines_with_entries: Vec<(String, Option<OperationCompletionEntry>)> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let entry = serde_json::from_str::<OperationCompletionEntry>(line).ok();
            (line.to_string(), entry)
        })
        .collect();

    let mut removed = 0usize;
    let mut kept_lines: Vec<&String> = Vec::with_capacity(lines_with_entries.len());
    for (raw_line, maybe_entry) in &lines_with_entries {
        let drop = match maybe_entry {
            Some(entry) => {
                let ts = entry.completed_at.parse::<u64>().unwrap_or(0);
                now_millis.saturating_sub(ts) > retention_millis
            }
            // Unparseable lines are preserved to avoid accidental data loss.
            None => false,
        };
        if drop {
            removed += 1;
        } else {
            kept_lines.push(raw_line);
        }
    }

    if removed == 0 {
        return Ok(0);
    }

    let temp_path = path.with_extension("jsonl.tmp");
    let parent = path
        .parent()
        .ok_or_else(|| format!("ledger path has no parent: {}", display_path(path)))?;
    fs::create_dir_all(parent).map_err(|e| format!("create dir {}: {e}", display_path(parent)))?;
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(|e| format!("open temp {}: {e}", display_path(&temp_path)))?;
        for line in &kept_lines {
            writeln!(file, "{line}")
                .map_err(|e| format!("write temp {}: {e}", display_path(&temp_path)))?;
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

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_completion_entry(
    app: AppHandle,
    operation_id: String,
) -> Result<Option<OperationCompletionEntry>, String> {
    completion_entry_for(&app, &operation_id)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn append_completion_entry_command(
    app: AppHandle,
    entry: OperationCompletionEntry,
) -> Result<(), String> {
    append_completion_entry(&app, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{mpsc, Arc, Barrier},
        time::Duration,
    };

    fn temp_ledger_path(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("mnemosyne-completion-ledger-{name}-{suffix}"))
            .join(OPERATION_COMPLETION_LEDGER_FILE)
    }

    #[test]
    fn append_and_lookup_round_trip() {
        let path = temp_ledger_path("round-trip");
        let entry = OperationCompletionEntry::new(
            "op-1",
            "graph.importArchive",
            Some("graph-abc".to_string()),
            "1735000000000",
        )
        .with_payload_hash("deadbeef")
        .with_result(serde_json::json!({ "documentsCreated": 5 }));

        append_completion_entry_at_path(&path, entry.clone()).expect("append entry");

        let found = completion_entry_for_at_path(&path, "op-1").expect("lookup");
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.operation_id, "op-1");
        assert_eq!(found.kind, "graph.importArchive");
        assert_eq!(found.graph_id, Some("graph-abc".to_string()));
        assert_eq!(found.payload_hash, Some("deadbeef".to_string()));
        assert_eq!(
            found.result,
            Some(serde_json::json!({ "documentsCreated": 5 }))
        );

        let _ = fs::remove_dir_all(path.parent().expect("ledger parent"));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let path = temp_ledger_path("missing");
        // File does not exist yet
        let found = completion_entry_for_at_path(&path, "op-unknown").expect("lookup");
        assert!(found.is_none());

        // After appending some other entry, the unknown one is still None
        let entry =
            OperationCompletionEntry::new("op-other", "import.vault", None, "1735000000000");
        append_completion_entry_at_path(&path, entry).expect("append");
        let found = completion_entry_for_at_path(&path, "op-unknown").expect("lookup");
        assert!(found.is_none());

        let _ = fs::remove_dir_all(path.parent().expect("ledger parent"));
    }

    #[test]
    fn most_recent_wins_for_duplicate_operation_id() {
        let path = temp_ledger_path("duplicate");
        let first = OperationCompletionEntry::new(
            "op-1",
            "graph.importArchive",
            Some("graph-1".to_string()),
            "1000",
        )
        .with_payload_hash("aaaa1111");
        let second = OperationCompletionEntry::new(
            "op-1",
            "graph.importArchive",
            Some("graph-1".to_string()),
            "2000",
        )
        .with_payload_hash("bbbb2222");
        append_completion_entry_at_path(&path, first).expect("append first");
        append_completion_entry_at_path(&path, second).expect("append second");

        let found = completion_entry_for_at_path(&path, "op-1")
            .expect("lookup")
            .unwrap();
        assert_eq!(found.completed_at, "2000");
        assert_eq!(found.payload_hash, Some("bbbb2222".to_string()));

        let _ = fs::remove_dir_all(path.parent().expect("ledger parent"));
    }

    #[test]
    fn prune_drops_entries_older_than_retention() {
        let path = temp_ledger_path("prune-old");
        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let eight_days_ago = now_millis - 8 * 86_400 * 1_000;
        let one_hour_ago = now_millis - 3_600 * 1_000;

        append_completion_entry_at_path(
            &path,
            OperationCompletionEntry::new(
                "op-old",
                "import.vault",
                None,
                eight_days_ago.to_string(),
            ),
        )
        .expect("append old");
        append_completion_entry_at_path(
            &path,
            OperationCompletionEntry::new(
                "op-recent",
                "graph.importArchive",
                None,
                one_hour_ago.to_string(),
            ),
        )
        .expect("append recent");

        let removed = prune_completion_ledger_at_path(&path, 7).expect("prune");
        assert_eq!(removed, 1);

        assert!(completion_entry_for_at_path(&path, "op-old")
            .expect("lookup")
            .is_none());
        assert!(completion_entry_for_at_path(&path, "op-recent")
            .expect("lookup")
            .is_some());

        let _ = fs::remove_dir_all(path.parent().expect("ledger parent"));
    }

    #[test]
    fn prune_handles_empty_or_missing_ledger() {
        let path = temp_ledger_path("empty");
        // Missing file
        assert_eq!(
            prune_completion_ledger_at_path(&path, 7).expect("prune missing"),
            0
        );

        // Empty file
        let parent = path.parent().expect("ledger parent");
        fs::create_dir_all(parent).expect("create dir");
        fs::write(&path, "").expect("create empty file");
        assert_eq!(
            prune_completion_ledger_at_path(&path, 7).expect("prune empty"),
            0
        );

        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn unparseable_lines_are_preserved_on_prune() {
        let path = temp_ledger_path("unparseable");
        let parent = path.parent().expect("ledger parent");
        fs::create_dir_all(parent).expect("create dir");

        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let eight_days_ago = now_millis - 8 * 86_400 * 1_000;

        // Mix of: valid old (drop), valid recent (keep), garbage (preserve)
        let valid_old = serde_json::to_string(&OperationCompletionEntry::new(
            "op-old",
            "graph.importArchive",
            None,
            eight_days_ago.to_string(),
        ))
        .unwrap();
        let valid_recent = serde_json::to_string(&OperationCompletionEntry::new(
            "op-recent",
            "graph.importArchive",
            None,
            now_millis.to_string(),
        ))
        .unwrap();
        let garbage = "not json at all";

        fs::write(&path, format!("{valid_old}\n{garbage}\n{valid_recent}\n")).expect("seed file");

        let removed = prune_completion_ledger_at_path(&path, 7).expect("prune");
        assert_eq!(removed, 1);

        let kept = fs::read_to_string(&path).expect("read");
        assert!(kept.contains("op-recent"));
        assert!(kept.contains(garbage));
        assert!(!kept.contains("op-old"));

        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn permissive_parsing_skips_unknown_schema_versions() {
        let path = temp_ledger_path("schema");
        let parent = path.parent().expect("ledger parent");
        fs::create_dir_all(parent).expect("create dir");

        let future_entry = serde_json::json!({
            "schemaVersion": 99,
            "operationId": "op-future",
            "kind": "graph.importArchive",
            "completedAt": "1000",
        });
        fs::write(&path, format!("{}\n", future_entry)).expect("seed");

        // Unknown schema version: lookup returns None (we cannot trust the shape)
        let found = completion_entry_for_at_path(&path, "op-future").expect("lookup");
        assert!(found.is_none());

        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn concurrent_appends_remain_distinct_parseable_records() {
        let path = temp_ledger_path("concurrent-appends");
        let writers = 24usize;
        let barrier = Arc::new(Barrier::new(writers));
        let mut handles = Vec::new();
        for index in 0..writers {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                append_completion_entry_at_path(
                    &path,
                    OperationCompletionEntry::new(
                        format!("op-concurrent-{index}"),
                        "import.vault",
                        None,
                        index.to_string(),
                    ),
                )
            }));
        }
        for handle in handles {
            handle.join().expect("append thread").expect("append entry");
        }

        let raw = fs::read_to_string(&path).expect("read concurrent ledger");
        let lines = raw.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), writers);
        assert!(lines
            .iter()
            .all(|line| { serde_json::from_str::<OperationCompletionEntry>(line).is_ok() }));
        for index in 0..writers {
            assert!(
                completion_entry_for_at_path(&path, &format!("op-concurrent-{index}"))
                    .expect("lookup concurrent entry")
                    .is_some()
            );
        }
        let _ = fs::remove_dir_all(path.parent().expect("ledger parent"));
    }

    #[test]
    fn append_waits_for_prune_rewrite_and_survives_rename() {
        let path = temp_ledger_path("prune-vs-append");
        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        append_completion_entry_at_path(
            &path,
            OperationCompletionEntry::new(
                "op-old-prune-race",
                "import.vault",
                None,
                (now_millis - 8 * 86_400 * 1_000).to_string(),
            ),
        )
        .expect("append old entry");

        let (reached_tx, reached_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        crate::jsonl_mutation_lock::install_jsonl_lock_hold_hook_for_test(
            &path, reached_tx, resume_rx,
        )
        .expect("install held prune lock");
        let prune_path = path.clone();
        let prune = std::thread::spawn(move || prune_completion_ledger_at_path(&prune_path, 7));
        let expected_lock =
            crate::jsonl_mutation_lock::jsonl_mutation_lock_path(&path).expect("JSONL lock path");
        assert_eq!(
            reached_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("prune acquired JSONL lock"),
            expected_lock
        );

        let append_path = path.clone();
        let (append_done_tx, append_done_rx) = mpsc::channel();
        let append = std::thread::spawn(move || {
            let result = append_completion_entry_at_path(
                &append_path,
                OperationCompletionEntry::new(
                    "op-appended-during-prune",
                    "graph.importArchive",
                    None,
                    now_millis.to_string(),
                ),
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
        assert_eq!(prune.join().expect("prune thread").expect("prune"), 1);
        append.join().expect("append thread").expect("append");

        assert!(completion_entry_for_at_path(&path, "op-old-prune-race")
            .expect("lookup old")
            .is_none());
        assert!(
            completion_entry_for_at_path(&path, "op-appended-during-prune")
                .expect("lookup appended")
                .is_some(),
            "append after atomic rename must land in the replacement inode"
        );
        crate::jsonl_mutation_lock::clear_jsonl_lock_hold_hook_for_test(&path);
        let _ = fs::remove_dir_all(path.parent().expect("ledger parent"));
    }
}
