use crate::{
    app_error::{AppError, AppResult},
    graph_record_store::GraphRecord,
    ids::validate_local_id,
    profile_metadata_db::{
        profile_metadata_identity, text_column, with_profile_metadata_connection,
        with_profile_metadata_worker,
    },
    runtime_config::GRAPH_STATUS_DELETED,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use turso::{params, Connection};

/// The last record JSON each worker committed for `(profile, graph)`, with the
/// identity of the catalogue file it was committed to. Written only by the
/// profile's catalogue worker, after the upsert succeeded, so its order is the
/// database's order; read by callers to skip an upsert that would rewrite the
/// row it already holds. A replaced or removed catalogue file has another
/// identity, so its entries are never trusted.
type UpsertedKey = (PathBuf, String);
static UPSERTED: OnceLock<Mutex<HashMap<UpsertedKey, (u64, String)>>> = OnceLock::new();

fn upserted() -> std::sync::MutexGuard<'static, HashMap<UpsertedKey, (u64, String)>> {
    UPSERTED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether the catalogue already holds exactly this record, as committed by this
/// process. A stat and a string compare; no worker, no write guard.
fn catalog_row_is_current(profile_dir: &Path, graph: &GraphRecord, record_json: &str) -> bool {
    let Some(identity) = profile_metadata_identity(profile_dir) else {
        return false;
    };
    upserted()
        .get(&(profile_dir.to_path_buf(), graph.graph_id.clone()))
        .is_some_and(|(committed_identity, committed)| {
            *committed_identity == identity && committed == record_json
        })
}

pub(crate) fn load_cached_profile_graph_records(
    profile_dir: &Path,
    include_deleted: bool,
) -> AppResult<Vec<GraphRecord>> {
    with_profile_metadata_connection(profile_dir, move |conn| async move {
        let mut records = if include_deleted {
            query_graph_records(&conn, None).await?
        } else {
            query_graph_records(&conn, Some(GRAPH_STATUS_DELETED)).await?
        };
        records.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.graph_id.cmp(&right.graph_id))
        });
        Ok(records)
    })
}

pub(crate) fn upsert_cached_profile_graph_record(
    profile_dir: &Path,
    graph: &GraphRecord,
) -> AppResult<()> {
    // `graph.json` is canonical and this row is its cache. A hot write path reads
    // the record on every enqueue; when the cache already holds these exact
    // bytes there is nothing to write, and nothing to wait for.
    let record_json = serde_json::to_string(graph).map_err(|error| {
        AppError::serialization(format!("serialize cached graph record: {error}"))
    })?;
    let _span = tracing::info_span!(target: "gardend_otel", "graph.catalog.upsert",
        sophia.graph.id = %graph.graph_id)
    .entered();
    if catalog_row_is_current(profile_dir, graph, &record_json) {
        tracing::info!(target: "gardend_otel", { sophia.catalog.skipped_unchanged = true }, "catalog row current");
        return Ok(());
    }
    let graph = graph.clone();
    let key_dir = profile_dir.to_path_buf();
    with_profile_metadata_worker(profile_dir, move |ctx| {
        let conn = ctx.connection()?;
        let identity = ctx.identity()?;
        ctx.block_on(upsert_graph_record_with_connection(&conn, &graph))??;
        upserted().insert((key_dir, graph.graph_id.clone()), (identity, record_json));
        Ok(())
    })
}

pub(crate) fn delete_cached_profile_graph_record(
    profile_dir: &Path,
    graph_id: &str,
) -> AppResult<()> {
    validate_local_id(graph_id, "graph_id").map_err(AppError::validation)?;
    let graph_id = graph_id.to_string();
    let key_dir = profile_dir.to_path_buf();
    with_profile_metadata_worker(profile_dir, move |ctx| {
        let conn = ctx.connection()?;
        upserted().remove(&(key_dir, graph_id.clone()));
        ctx.block_on(async {
            conn.execute(
                "DELETE FROM graph_records WHERE graph_id = ?1",
                params![graph_id],
            )
            .await
            .map_err(|error| AppError::database(format!("delete cached graph record: {error}")))
        })??;
        Ok(())
    })
}

pub(crate) fn profile_dir_from_graph_dir(graph_dir: &Path) -> AppResult<PathBuf> {
    let graphs_dir = graph_dir.parent().ok_or_else(|| {
        AppError::validation(format!("graph dir has no parent: {}", graph_dir.display()))
    })?;
    if graphs_dir.file_name().and_then(|value| value.to_str()) != Some("graphs") {
        return Err(AppError::validation(format!(
            "graph dir is not inside a graphs directory: {}",
            graph_dir.display()
        )));
    }
    graphs_dir.parent().map(Path::to_path_buf).ok_or_else(|| {
        AppError::validation(format!(
            "graphs dir has no profile parent: {}",
            graphs_dir.display()
        ))
    })
}

async fn query_graph_records(
    conn: &Connection,
    deleted_status_filter: Option<&str>,
) -> AppResult<Vec<GraphRecord>> {
    let mut rows = if let Some(deleted_status) = deleted_status_filter {
        conn.query(
            "SELECT record_json FROM graph_records WHERE status <> ?1 ORDER BY updated_at DESC, graph_id",
            params![deleted_status],
        )
        .await
        .map_err(|error| AppError::database(format!("query cached active graph records: {error}")))?
    } else {
        conn.query(
            "SELECT record_json FROM graph_records ORDER BY updated_at DESC, graph_id",
            (),
        )
        .await
        .map_err(|error| AppError::database(format!("query cached graph records: {error}")))?
    };

    let mut records = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| AppError::database(format!("read cached graph record row: {error}")))?
    {
        let record_json = text_column(&row, 0)?;
        match serde_json::from_str::<GraphRecord>(&record_json) {
            Ok(record) => records.push(record),
            Err(error) => {
                log::warn!("Skipping unreadable graph record from Turso cache: {error}");
            }
        }
    }
    Ok(records)
}

async fn upsert_graph_record_with_connection(
    conn: &Connection,
    graph: &GraphRecord,
) -> AppResult<()> {
    let record_json = serde_json::to_string(graph).map_err(|error| {
        AppError::serialization(format!("serialize cached graph record: {error}"))
    })?;
    conn.execute(
        r#"
INSERT INTO graph_records (
  graph_id,
  status,
  title,
  updated_at,
  created_at,
  record_json
) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(graph_id) DO UPDATE SET
  status = excluded.status,
  title = excluded.title,
  updated_at = excluded.updated_at,
  created_at = excluded.created_at,
  record_json = excluded.record_json
"#,
        params![
            graph.graph_id.clone(),
            graph.status.clone(),
            graph.title.clone(),
            graph.updated_at.clone(),
            graph.created_at.clone(),
            record_json,
        ],
    )
    .await
    .map_err(|error| AppError::database(format!("persist cached graph record: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_config::{GRAPH_STATUS_ACTIVE, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_profile_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-profile-cache-{name}-{suffix}"))
    }

    fn graph_record(graph_id: &str, title: &str, status: &str, updated_at: &str) -> GraphRecord {
        GraphRecord {
            graph_id: graph_id.to_string(),
            title: title.to_string(),
            description: None,
            status: status.to_string(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{graph_id}"),
            created_at: "1000".to_string(),
            incarnation_id: None,
            updated_at: updated_at.to_string(),
            capabilities: Vec::new(),
            created_by_operation_id: None,
            validation_policy: crate::runtime_config::ValidationPolicy::default(),
            content_revision: None,
        }
    }

    #[test]
    fn graph_catalog_cache_round_trips_active_records() {
        let profile_dir = temp_profile_dir("roundtrip");
        let active = graph_record("graph-a", "Graph A", GRAPH_STATUS_ACTIVE, "2000");
        let newer = graph_record("graph-b", "Graph B", GRAPH_STATUS_ACTIVE, "3000");
        let deleted = graph_record("graph-c", "Graph C", GRAPH_STATUS_DELETED, "4000");

        upsert_cached_profile_graph_record(&profile_dir, &active).unwrap();
        upsert_cached_profile_graph_record(&profile_dir, &newer).unwrap();
        upsert_cached_profile_graph_record(&profile_dir, &deleted).unwrap();

        let active_records = load_cached_profile_graph_records(&profile_dir, false).unwrap();
        assert_eq!(
            active_records
                .iter()
                .map(|graph| graph.graph_id.as_str())
                .collect::<Vec<_>>(),
            vec!["graph-b", "graph-a"]
        );

        let all_records = load_cached_profile_graph_records(&profile_dir, true).unwrap();
        assert_eq!(all_records.len(), 3);
        assert_eq!(all_records[0].graph_id, "graph-c");

        delete_cached_profile_graph_record(&profile_dir, "graph-b").unwrap();
        let active_records = load_cached_profile_graph_records(&profile_dir, false).unwrap();
        assert_eq!(active_records.len(), 1);
        assert_eq!(active_records[0].graph_id, "graph-a");

        let _ = std::fs::remove_dir_all(profile_dir);
    }

    #[test]
    fn catalog_worker_opens_the_database_once_for_a_burst_of_reads_and_writes() {
        use crate::profile_metadata_db::profile_metadata_open_count;
        let profile_dir = temp_profile_dir("opens-once");
        let mut record = graph_record("graph-a", "Graph A", GRAPH_STATUS_ACTIVE, "2000");

        for step in 0..25 {
            // The same record again and again (what every enqueue does), and a
            // changed one now and then (what every content write does).
            if step % 5 == 0 {
                record.updated_at = format!("{}", 2000 + step);
            }
            upsert_cached_profile_graph_record(&profile_dir, &record).unwrap();
            let loaded = load_cached_profile_graph_records(&profile_dir, true).unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].updated_at, record.updated_at);
        }
        assert_eq!(
            profile_metadata_open_count(&profile_dir),
            1,
            "one database open (and one DDL pass) for the whole burst"
        );

        // A removed row is written again even though the record bytes are the
        // ones last committed: the skip cache must not outlive the delete.
        delete_cached_profile_graph_record(&profile_dir, "graph-a").unwrap();
        assert!(load_cached_profile_graph_records(&profile_dir, true)
            .unwrap()
            .is_empty());
        upsert_cached_profile_graph_record(&profile_dir, &record).unwrap();
        assert_eq!(
            load_cached_profile_graph_records(&profile_dir, true)
                .unwrap()
                .len(),
            1
        );

        // A replaced catalogue file (restore, deleted profile) is noticed: the
        // worker reopens and the row is committed to the new file.
        std::fs::remove_dir_all(&profile_dir).unwrap();
        upsert_cached_profile_graph_record(&profile_dir, &record).unwrap();
        assert_eq!(
            load_cached_profile_graph_records(&profile_dir, true)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(profile_metadata_open_count(&profile_dir), 2);

        let _ = std::fs::remove_dir_all(profile_dir);
    }

    #[test]
    fn unchanged_upsert_does_not_touch_the_catalogue() {
        let profile_dir = temp_profile_dir("skip");
        let record = graph_record("graph-a", "Graph A", GRAPH_STATUS_ACTIVE, "2000");
        upsert_cached_profile_graph_record(&profile_dir, &record).unwrap();
        let db_file = profile_dir.join("metadata.turso");
        let before = std::fs::metadata(&db_file).unwrap().modified().unwrap();
        let wal = profile_dir.join("metadata.turso-wal");
        let wal_before = std::fs::metadata(&wal)
            .ok()
            .map(|m| (m.len(), m.modified().ok()));
        std::thread::sleep(std::time::Duration::from_millis(30));
        for _ in 0..10 {
            upsert_cached_profile_graph_record(&profile_dir, &record).unwrap();
        }
        assert_eq!(
            std::fs::metadata(&db_file).unwrap().modified().unwrap(),
            before
        );
        assert_eq!(
            std::fs::metadata(&wal)
                .ok()
                .map(|m| (m.len(), m.modified().ok())),
            wal_before,
            "an unchanged record writes nothing"
        );
        let _ = std::fs::remove_dir_all(profile_dir);
    }

    #[test]
    fn profile_dir_from_graph_dir_requires_graphs_parent() {
        let profile_dir = temp_profile_dir("path");
        let graph_dir = profile_dir.join("graphs").join("graph-a");

        assert_eq!(profile_dir_from_graph_dir(&graph_dir).unwrap(), profile_dir);

        let error = profile_dir_from_graph_dir(&PathBuf::from("/tmp/not-graphs/graph-a"))
            .expect_err("unexpected graph directory should fail");
        assert!(error.to_string().contains("not inside a graphs directory"));
    }
}
