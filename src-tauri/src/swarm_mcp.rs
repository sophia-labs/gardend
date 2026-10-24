//! MCP seam for analysis-cell snapshots. Dispatch runs these blocking reads/imports
//! on the blocking pool, never in the per-graph CRDT mutation queue.
use crate::{app_runtime::AppHandle, swarm_read, swarm_store};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, sync::Arc};

fn required<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{name} is required and must be a nonempty string"))
}

fn strings(args: &Value, name: &str) -> Result<Option<Vec<String>>, String> {
    args.get(name)
        .map(|v| serde_json::from_value(v.clone()).map_err(|e| format!("{name}: {e}")))
        .transpose()
}

fn limit(args: &Value, key: &str, default: usize, max: usize) -> Result<usize, String> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|v| *v > 0 && *v <= max as u64)
            .map(|v| v as usize)
            .ok_or_else(|| format!("{key} must be between 1 and {max}")),
    }
}

pub(crate) fn call(app: AppHandle, args: &Value, operation: &str) -> Result<Value, String> {
    let graph = crate::mcp_utils::mcp_graph_id_or_default(&app, args)?;
    let (directory, record) = crate::graph_record_store::read_graph_record_no_heal(&app, &graph)
        .map_err(|e| e.to_string())?;
    let incarnation = record
        .incarnation_id
        .ok_or("swarm graph incarnation is missing")?;
    let cell = swarm_store::cell(&directory, &incarnation)?;
    let excludes = strings(args, "exclude_graphs")?.unwrap_or_default();
    let mask = swarm_read::Mask::new(&excludes)?;
    match operation {
        "swarm_load" => {
            let snapshot = cell.import(
                Path::new(required(args, "rdf_path")?),
                Path::new(required(args, "embeddings_manifest_path")?),
            )?;
            Ok(json!({"loaded": true, "snapshot": snapshot.generation}))
        }
        "swarm_save_query" => {
            let name = required(args, "name")?;
            if name.len() > 128
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(
                    "saved query name uses at most 128 ASCII letters/digits/underscore/hyphen"
                        .into(),
                );
            }
            let query = required(args, "query")?;
            if query.len() > 100_000 {
                return Err("saved query exceeds 100000 bytes".into());
            }
            swarm_read::validate_template(query)?;
            let _writer = cell
                .writer
                .lock()
                .map_err(|_| "swarm import lock poisoned")?;
            let mut queries =
                (**cell.queries.read().map_err(|_| "swarm queries poisoned")?).clone();
            queries.insert(name.to_owned(), query.to_owned());
            swarm_store::write_json(&cell.directory.join("queries.json"), &queries)?;
            *cell.queries.write().map_err(|_| "swarm queries poisoned")? = Arc::new(queries);
            Ok(json!({"saved": true, "name": name}))
        }
        "swarm_query" => {
            let snapshot = cell.snapshot()?;
            swarm_read::query(
                snapshot.as_ref(),
                &mask,
                required(args, "query")?,
                limit(args, "max_rows", 10_000, 50_000)?,
            )
        }
        "swarm_query_named" => {
            let queries = {
                let guard = cell.queries.read().map_err(|_| "swarm queries poisoned")?;
                Arc::clone(&*guard)
            };
            let template = queries
                .get(required(args, "name")?)
                .ok_or("unknown saved swarm query")?;
            let parameters: BTreeMap<String, Value> = match args.get("parameters") {
                Some(value) => serde_json::from_value(value.clone()).map_err(|e| e.to_string())?,
                None => BTreeMap::new(),
            };
            let query = swarm_read::render(template, &parameters)?;
            let snapshot = cell.snapshot()?;
            swarm_read::query(
                snapshot.as_ref(),
                &mask,
                &query,
                limit(args, "max_rows", 10_000, 50_000)?,
            )
        }
        "swarm_semantic_search" => {
            let vector: Vec<f32> =
                serde_json::from_value(args.get("vector").ok_or("vector is required")?.clone())
                    .map_err(|e| e.to_string())?;
            let kind = args
                .get("kind")
                .map(|v| v.as_str().ok_or("kind must be a string"))
                .transpose()?;
            let allowed = strings(args, "allowed_ids")?;
            let snapshot = cell.snapshot()?;
            swarm_read::semantic(
                snapshot.as_ref(),
                &mask,
                &vector,
                kind,
                limit(args, "k", 8, 100)?,
                allowed.as_deref(),
            )
        }
        "swarm_traverse" => {
            let predicate = args
                .get("predicate")
                .map(|v| v.as_str().ok_or("predicate must be an IRI string"))
                .transpose()?;
            let snapshot = cell.snapshot()?;
            swarm_read::traverse(
                snapshot.as_ref(),
                &mask,
                required(args, "start")?,
                predicate,
                limit(args, "depth", 1, 6)?,
                limit(args, "limit", 100, 10_000)?,
            )
        }
        _ => Err("unknown swarm operation".into()),
    }
}
