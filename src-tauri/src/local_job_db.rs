use crate::storage::{create_dir_all, display_path};
use std::{future::Future, path::Path, sync::Mutex};
use turso::{Connection, Row, Value};

static LOCAL_JOB_DB_LOCK: Mutex<()> = Mutex::new(());

const LOCAL_JOB_DB_FILE: &str = "jobs.turso";

pub(super) fn with_job_connection<R, Fut, F>(jobs_dir: &Path, operation: F) -> Result<R, String>
where
    R: Send + 'static,
    Fut: Future<Output = Result<R, String>> + Send + 'static,
    F: FnOnce(Connection) -> Fut + Send + 'static,
{
    let _flush_guard = crate::cell_durability::write_guard();
    let jobs_dir = jobs_dir.to_path_buf();
    std::thread::spawn(move || {
        let _guard = LOCAL_JOB_DB_LOCK
            .lock()
            .map_err(|_| "local job Turso cache lock poisoned".to_string())?;
        create_dir_all(&jobs_dir)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("create local job Turso runtime: {error}"))?;
        runtime.block_on(async move {
            let conn = open_job_connection(&jobs_dir).await?;
            initialize_job_store(&conn).await?;
            operation(conn).await
        })
    })
    .join()
    .map_err(|_| "local job Turso cache worker panicked".to_string())?
}

pub(super) fn text_column(row: &Row, idx: usize) -> Result<String, String> {
    match row
        .get_value(idx)
        .map_err(|error| format!("read local job text column {idx}: {error}"))?
    {
        Value::Text(value) => Ok(value),
        other => Err(format!(
            "local job text column {idx} had unexpected value {other:?}"
        )),
    }
}

async fn open_job_connection(jobs_dir: &Path) -> Result<Connection, String> {
    let db_path = jobs_dir.join(LOCAL_JOB_DB_FILE);
    let db_path = display_path(&db_path);
    let db = turso::Builder::new_local(&db_path)
        .build()
        .await
        .map_err(|error| format!("open local job Turso cache {db_path}: {error}"))?;
    db.connect()
        .map_err(|error| format!("connect local job Turso cache {db_path}: {error}"))
}

async fn initialize_job_store(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS local_jobs (
  job_id TEXT PRIMARY KEY,
  status TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  submitted_at TEXT NOT NULL,
  graph_id TEXT,
  job_type TEXT,
  record_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_local_jobs_status_updated
  ON local_jobs(status, updated_at);
CREATE INDEX IF NOT EXISTS idx_local_jobs_graph_updated
  ON local_jobs(graph_id, updated_at);
"#,
    )
    .await
    .map_err(|error| format!("initialize local job Turso cache: {error}"))
}
