use crate::app_runtime::AppHandle;
use crate::{
    graph_record_store::read_graph_record, graph_service::list_graphs, paths::graphs_dir,
    rdf_service::open_graph_store, runtime_config::PROFILE_ID, storage::display_path,
};
use std::{fs, path::Path};

fn directory_size_bytes(path: &Path) -> Result<u64, String> {
    if !path.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("read metadata for {}: {error}", display_path(path)))?;
    if metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }

    let mut total = 0_u64;
    for entry in fs::read_dir(path)
        .map_err(|error| format!("read directory {}: {error}", display_path(path)))?
    {
        let entry = entry.map_err(|error| format!("read directory entry: {error}"))?;
        total = total.saturating_add(directory_size_bytes(&entry.path())?);
    }
    Ok(total)
}

pub(crate) fn local_graph_storage_usage(
    app: &AppHandle,
    graph_id: &str,
) -> Result<serde_json::Value, String> {
    let (graph_dir, _) = read_graph_record(app, graph_id)?;
    let used_bytes = directory_size_bytes(&graph_dir)?;
    Ok(serde_json::json!({
        "user_id": PROFILE_ID,
        "graph_id": graph_id,
        "tier": "local",
        "used_bytes": used_bytes,
        "bytes_used": used_bytes,
        "limit_bytes": serde_json::Value::Null,
        "warning_bytes": serde_json::Value::Null,
        "usage_ratio": 0.0,
        "warning_threshold_reached": false,
        "limit_reached": false,
    }))
}

pub(crate) fn hosted_graph_stats(app: AppHandle) -> Result<serde_json::Value, String> {
    let graphs = list_graphs(app.clone())?;
    let mut total_triples = 0_u64;
    for graph in &graphs {
        let graph_dir = graphs_dir(&app)?.join(&graph.graph_id);
        let store = open_graph_store(&graph_dir)?;
        let quad_count = store
            .len()
            .map_err(|error| format!("count graph quads for {}: {error}", graph.graph_id))?;
        total_triples = total_triples.saturating_add(quad_count as u64);
    }
    let total_graphs = graphs.len() as u64;
    let avg_triples = if total_graphs == 0 {
        0.0
    } else {
        total_triples as f64 / total_graphs as f64
    };
    Ok(serde_json::json!({
        "total_graphs": total_graphs,
        "total_triples": total_triples,
        "avg_triples": avg_triples,
    }))
}
