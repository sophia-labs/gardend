use crate::{
    app_error::{AppError, AppResult},
    graph_record_store::GraphRecord,
    ids::validate_local_id,
    profile_metadata_db::{text_column, with_profile_metadata_connection},
    runtime_config::GRAPH_STATUS_DELETED,
};
use std::path::{Path, PathBuf};
use turso::{params, Connection};

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
    let graph = graph.clone();
    with_profile_metadata_connection(profile_dir, move |conn| async move {
        upsert_graph_record_with_connection(&conn, &graph).await
    })
}

pub(crate) fn delete_cached_profile_graph_record(
    profile_dir: &Path,
    graph_id: &str,
) -> AppResult<()> {
    validate_local_id(graph_id, "graph_id").map_err(AppError::validation)?;
    let graph_id = graph_id.to_string();
    with_profile_metadata_connection(profile_dir, move |conn| async move {
        conn.execute(
            "DELETE FROM graph_records WHERE graph_id = ?1",
            params![graph_id],
        )
        .await
        .map_err(|error| AppError::database(format!("delete cached graph record: {error}")))?;
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
    fn profile_dir_from_graph_dir_requires_graphs_parent() {
        let profile_dir = temp_profile_dir("path");
        let graph_dir = profile_dir.join("graphs").join("graph-a");

        assert_eq!(profile_dir_from_graph_dir(&graph_dir).unwrap(), profile_dir);

        let error = profile_dir_from_graph_dir(&PathBuf::from("/tmp/not-graphs/graph-a"))
            .expect_err("unexpected graph directory should fail");
        assert!(error.to_string().contains("not inside a graphs directory"));
    }
}
