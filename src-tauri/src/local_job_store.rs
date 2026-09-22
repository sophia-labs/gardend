use crate::{
    ids::validate_local_id,
    local_job_db::{text_column, with_job_connection},
    local_job_types::{local_job_graph_id, LocalJobRecord},
    storage::create_dir_all,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use turso::{params, Connection};

pub(super) fn load_job_records(
    jobs_dir: &Path,
) -> Result<BTreeMap<String, LocalJobRecord>, String> {
    with_job_connection(jobs_dir, |conn| async move {
        let mut rows = conn
            .query(
                "SELECT record_json FROM local_jobs ORDER BY submitted_at, job_id",
                (),
            )
            .await
            .map_err(|error| format!("query local jobs: {error}"))?;
        let mut records = BTreeMap::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| format!("read local job row: {error}"))?
        {
            let record_json = text_column(&row, 0)?;
            match serde_json::from_str::<LocalJobRecord>(&record_json) {
                Ok(record) => {
                    records.insert(record.job_id.clone(), record);
                }
                Err(error) => {
                    log::warn!("Skipping unreadable local job record from Turso cache: {error}");
                }
            }
        }
        Ok(records)
    })
}

pub(super) fn persist_job_record(jobs_dir: &Path, record: &LocalJobRecord) -> Result<(), String> {
    let record = record.clone();
    with_job_connection(jobs_dir, move |conn| async move {
        persist_job_record_with_connection(&conn, &record).await
    })
}

pub(super) fn read_job_record(
    jobs_dir: &Path,
    job_id: &str,
) -> Result<Option<LocalJobRecord>, String> {
    validate_local_id(job_id, "job_id")?;
    let job_id = job_id.to_string();
    with_job_connection(jobs_dir, move |conn| async move {
        let mut rows = conn
            .query(
                "SELECT record_json FROM local_jobs WHERE job_id = ?1 LIMIT 1",
                params![job_id],
            )
            .await
            .map_err(|error| format!("query local job record: {error}"))?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|error| format!("read local job record row: {error}"))?
        else {
            return Ok(None);
        };
        let record_json = text_column(&row, 0)?;
        serde_json::from_str::<LocalJobRecord>(&record_json)
            .map(Some)
            .map_err(|error| format!("parse local job record: {error}"))
    })
}

pub(super) fn ensure_job_dir(jobs_dir: &Path, job_id: &str) -> Result<PathBuf, String> {
    let job_dir = job_dir(jobs_dir, job_id)?;
    create_dir_all(&job_dir)?;
    Ok(job_dir)
}

pub(super) fn job_dir(jobs_dir: &Path, job_id: &str) -> Result<PathBuf, String> {
    validate_local_id(job_id, "job_id")?;
    Ok(jobs_dir.join(job_id))
}

pub(super) fn result_path(jobs_dir: &Path, job_id: &str) -> Result<PathBuf, String> {
    Ok(job_dir(jobs_dir, job_id)?.join("result.json"))
}

async fn persist_job_record_with_connection(
    conn: &Connection,
    record: &LocalJobRecord,
) -> Result<(), String> {
    let record_json = serde_json::to_string(record)
        .map_err(|error| format!("serialize local job record: {error}"))?;
    let graph_id = local_job_graph_id(record);
    let job_type = record
        .detail
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    conn.execute(
        r#"
INSERT INTO local_jobs (
  job_id,
  status,
  updated_at,
  submitted_at,
  graph_id,
  job_type,
  record_json
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(job_id) DO UPDATE SET
  status = excluded.status,
  updated_at = excluded.updated_at,
  submitted_at = excluded.submitted_at,
  graph_id = excluded.graph_id,
  job_type = excluded.job_type,
  record_json = excluded.record_json
"#,
        params![
            record.job_id.clone(),
            serde_json::to_string(&record.status)
                .map_err(|error| format!("serialize job status: {error}"))?
                .trim_matches('"')
                .to_string(),
            record.updated_at.clone(),
            record.submitted_at.clone(),
            graph_id,
            job_type,
            record_json,
        ],
    )
    .await
    .map_err(|error| format!("persist local job record: {error}"))?;
    Ok(())
}
