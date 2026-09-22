use crate::{
    app_error::{AppError, AppResult},
    storage::{create_dir_all, display_path},
};
use std::{future::Future, path::Path, sync::Mutex};
use turso::{Connection, Row, Value};

static PROFILE_METADATA_DB_LOCK: Mutex<()> = Mutex::new(());

const PROFILE_METADATA_DB_FILE: &str = "metadata.turso";

pub(crate) fn with_profile_metadata_connection<R, Fut, F>(
    profile_dir: &Path,
    operation: F,
) -> AppResult<R>
where
    R: Send + 'static,
    Fut: Future<Output = AppResult<R>> + Send + 'static,
    F: FnOnce(Connection) -> Fut + Send + 'static,
{
    let _flush_guard = crate::cell_durability::write_guard();
    let profile_dir = profile_dir.to_path_buf();
    std::thread::spawn(move || {
        let _guard = PROFILE_METADATA_DB_LOCK
            .lock()
            .map_err(|_| AppError::internal("profile metadata Turso cache lock poisoned"))?;
        create_dir_all(&profile_dir).map_err(AppError::storage)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                AppError::internal(format!("create profile metadata Turso runtime: {error}"))
            })?;
        runtime.block_on(async move {
            let conn = open_profile_metadata_connection(&profile_dir).await?;
            initialize_profile_metadata_store(&conn).await?;
            operation(conn).await
        })
    })
    .join()
    .map_err(|_| AppError::internal("profile metadata Turso cache worker panicked"))?
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

async fn open_profile_metadata_connection(profile_dir: &Path) -> AppResult<Connection> {
    let db_path = profile_dir.join(PROFILE_METADATA_DB_FILE);
    let db_path = display_path(&db_path);
    let db = turso::Builder::new_local(&db_path)
        .build()
        .await
        .map_err(|error| {
            AppError::database(format!(
                "open profile metadata Turso cache {db_path}: {error}"
            ))
        })?;
    db.connect().map_err(|error| {
        AppError::database(format!(
            "connect profile metadata Turso cache {db_path}: {error}"
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
