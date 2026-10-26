use crate::{
    app_error::{AppError, AppResult},
    storage::{create_dir_all, display_path},
};
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use turso::{Connection, Row, Value};

const PROFILE_METADATA_DB_FILE: &str = "metadata.turso";

/// How long a profile's catalogue worker stays open with no work. A burst of
/// writes opens the database once; an idle process holds no file open.
const WORKER_IDLE_EXIT: Duration = Duration::from_secs(10);

type Job = Box<dyn FnOnce(&mut WorkerCtx) + Send + 'static>;

/// One live worker. `state` holds the job sender while the worker accepts
/// work; the worker clears it (under the same lock that senders hold while
/// sending) when it exits idle, so a job is never queued to a worker that
/// is about to leave.
struct WorkerHandle {
    id: u64,
    state: Mutex<Option<mpsc::Sender<Job>>>,
}

static WORKERS: OnceLock<Mutex<HashMap<PathBuf, Arc<WorkerHandle>>>> = OnceLock::new();
static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
static OPEN_COUNTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

/// Number of times the catalogue database of `profile_dir` has been opened (and
/// its DDL run) by this process. A test hook: the count must stay at one for a
/// burst of reads and writes.
#[cfg(test)]
pub(crate) fn profile_metadata_open_count(profile_dir: &Path) -> usize {
    OPEN_COUNTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(profile_dir)
        .copied()
        .unwrap_or(0)
}

/// Identity of the catalogue file on disk. A replaced or removed file (a
/// restore, a deleted profile) changes it, and the worker then reopens; a
/// skip-cache entry recorded against another identity is not trusted.
pub(crate) fn profile_metadata_identity(profile_dir: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(profile_dir.join(PROFILE_METADATA_DB_FILE)).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Some(0)
    }
}

/// State owned by one profile's catalogue worker thread: its Tokio runtime and
/// the open database. Jobs run one at a time, in submission order.
pub(crate) struct WorkerCtx {
    profile_dir: PathBuf,
    runtime: Option<tokio::runtime::Runtime>,
    db: Option<(turso::Database, u64)>,
}

impl WorkerCtx {
    pub(crate) fn block_on<F: Future>(&self, future: F) -> AppResult<F::Output> {
        let runtime = self.runtime.as_ref().ok_or_else(|| {
            AppError::internal("create profile metadata Turso runtime: runtime unavailable")
        })?;
        Ok(runtime.block_on(future))
    }

    /// Identity of the open database file, after making sure it is open.
    pub(crate) fn identity(&mut self) -> AppResult<u64> {
        self.ensure_open()?;
        Ok(self.db.as_ref().map(|(_, identity)| *identity).unwrap_or(0))
    }

    /// A fresh connection on the open database, opening it (and creating the
    /// tables) first when this worker has not yet, or when the file changed.
    pub(crate) fn connection(&mut self) -> AppResult<Connection> {
        self.ensure_open()?;
        let (db, _) = self
            .db
            .as_ref()
            .ok_or_else(|| AppError::internal("profile metadata Turso cache is not open"))?;
        db.connect().map_err(|error| {
            AppError::database(format!("connect profile metadata Turso cache: {error}"))
        })
    }

    fn ensure_open(&mut self) -> AppResult<()> {
        let current = profile_metadata_identity(&self.profile_dir);
        if let (Some((_, open)), Some(current)) = (self.db.as_ref(), current) {
            if *open == current {
                return Ok(());
            }
        }
        self.db = None;
        create_dir_all(&self.profile_dir).map_err(AppError::storage)?;
        let profile_dir = self.profile_dir.clone();
        let db = self.block_on(async {
            let db = open_profile_metadata_database(&profile_dir).await?;
            let conn = db.connect().map_err(|error| {
                AppError::database(format!("connect profile metadata Turso cache: {error}"))
            })?;
            initialize_profile_metadata_store(&conn).await?;
            Ok::<_, AppError>(db)
        })??;
        let identity = profile_metadata_identity(&self.profile_dir).ok_or_else(|| {
            AppError::database("profile metadata Turso cache file missing after open")
        })?;
        #[cfg(test)]
        {
            *OPEN_COUNTS
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(self.profile_dir.clone())
                .or_insert(0) += 1;
        }
        self.db = Some((db, identity));
        Ok(())
    }
}

fn worker_main(profile_dir: PathBuf, handle: Arc<WorkerHandle>, receiver: mpsc::Receiver<Job>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| log::warn!("create profile metadata Turso runtime: {error}"))
        .ok();
    let mut ctx = WorkerCtx {
        profile_dir: profile_dir.clone(),
        runtime,
        db: None,
    };
    loop {
        match receiver.recv_timeout(WORKER_IDLE_EXIT) {
            Ok(job) => job(&mut ctx),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Senders send while holding `state`, so deciding to leave
                // under it cannot lose a job that arrives at the same moment.
                let mut state = handle
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match receiver.try_recv() {
                    Ok(job) => {
                        drop(state);
                        job(&mut ctx);
                    }
                    Err(_) => {
                        *state = None;
                        drop(state);
                        remove_worker(&profile_dir, handle.id);
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn remove_worker(profile_dir: &Path, id: u64) {
    let mut workers = WORKERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if workers
        .get(profile_dir)
        .is_some_and(|handle| handle.id == id)
    {
        workers.remove(profile_dir);
    }
}

fn worker_for(profile_dir: &Path) -> AppResult<Arc<WorkerHandle>> {
    let mut workers = WORKERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(handle) = workers.get(profile_dir) {
        return Ok(handle.clone());
    }
    let (sender, receiver) = mpsc::channel();
    let handle = Arc::new(WorkerHandle {
        id: NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed),
        state: Mutex::new(Some(sender)),
    });
    let thread_handle = handle.clone();
    let thread_dir = profile_dir.to_path_buf();
    std::thread::Builder::new()
        .name("profile-metadata".to_string())
        .spawn(move || worker_main(thread_dir, thread_handle, receiver))
        .map_err(|error| {
            AppError::internal(format!("spawn profile metadata Turso worker: {error}"))
        })?;
    workers.insert(profile_dir.to_path_buf(), handle.clone());
    Ok(handle)
}

fn submit(profile_dir: &Path, mut job: Job) -> AppResult<()> {
    for _ in 0..4 {
        let handle = worker_for(profile_dir)?;
        {
            let state = handle
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(sender) = state.as_ref() {
                match sender.send(job) {
                    Ok(()) => return Ok(()),
                    // The worker died (a panicking job): get the job back and
                    // start a fresh worker.
                    Err(mpsc::SendError(returned)) => job = returned,
                }
            }
        }
        // Either the worker left idle or it died; forget it and retry.
        {
            let mut state = handle
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *state = None;
        }
        remove_worker(profile_dir, handle.id);
    }
    Err(AppError::internal(
        "profile metadata Turso cache worker unavailable",
    ))
}

/// Run `operation` on the profile's catalogue worker and wait for its result.
///
/// The worker keeps one Tokio runtime and one open database per profile for as
/// long as it has work (it leaves after [`WORKER_IDLE_EXIT`] idle), so a call
/// costs a channel round trip, not a thread, a runtime, a database open and
/// two DDL statements. Jobs run in submission order. The caller, not the
/// worker, takes the durability write guard, exactly as before: the guard is
/// per thread, and a worker waiting for the flush gate while its caller held it
/// would deadlock.
pub(crate) fn with_profile_metadata_worker<R, F>(profile_dir: &Path, operation: F) -> AppResult<R>
where
    R: Send + 'static,
    F: FnOnce(&mut WorkerCtx) -> AppResult<R> + Send + 'static,
{
    let _flush_guard = crate::cell_durability::write_guard();
    let (reply, wait) = mpsc::channel();
    let job: Job = Box::new(move |ctx| {
        let _ = reply.send(operation(ctx));
    });
    submit(profile_dir, job)?;
    wait.recv()
        .map_err(|_| AppError::internal("profile metadata Turso cache worker panicked"))?
}

pub(crate) fn with_profile_metadata_connection<R, Fut, F>(
    profile_dir: &Path,
    operation: F,
) -> AppResult<R>
where
    R: Send + 'static,
    Fut: Future<Output = AppResult<R>> + Send + 'static,
    F: FnOnce(Connection) -> Fut + Send + 'static,
{
    with_profile_metadata_worker(profile_dir, move |ctx| {
        let conn = ctx.connection()?;
        ctx.block_on(operation(conn))?
    })
}

pub(crate) fn text_column(row: &Row, idx: usize) -> AppResult<String> {
    match row.get_value(idx).map_err(|error| {
        AppError::database(format!("read profile metadata text column {idx}: {error}"))
    })? {
        Value::Text(value) => Ok(value),
        other => Err(AppError::database(format!(
            "profile metadata text column {idx} had unexpected value {other:?}"
        ))),
    }
}

async fn open_profile_metadata_database(profile_dir: &Path) -> AppResult<turso::Database> {
    let db_path = profile_dir.join(PROFILE_METADATA_DB_FILE);
    let db_path = display_path(&db_path);
    turso::Builder::new_local(&db_path)
        .build()
        .await
        .map_err(|error| {
            AppError::database(format!(
                "open profile metadata Turso cache {db_path}: {error}"
            ))
        })
}

async fn initialize_profile_metadata_store(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS graph_records (
  graph_id TEXT PRIMARY KEY,
  status TEXT NOT NULL,
  title TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  created_at TEXT NOT NULL,
  record_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_graph_records_status_updated
  ON graph_records(status, updated_at);
"#,
    )
    .await
    .map_err(|error| {
        AppError::database(format!("initialize profile metadata Turso cache: {error}"))
    })
}
