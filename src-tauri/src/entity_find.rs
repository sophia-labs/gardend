//! MCP `find`: entities of a graph's own RDF, by meaning and by words, for
//! agents. Results are short (≈100 tokens a hit), roll parts up to what they
//! are part of, name where each fact comes from, and say what to call next.

use crate::app_error::{AppError, AppResult};
use crate::app_runtime::AppHandle;
use crate::entity_embedder::with_entity_embedder;
use crate::entity_index::{
    entity_index_enabled, load_entity_index, FindFilter, FindMode, LoadedEntityIndex, RankedHit,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Instant;

const DEFAULT_K: usize = 5;
const MAX_K: usize = 25;

pub(crate) fn mcp_local_find(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id =
        crate::mcp_arg_utils::mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    crate::ids::validate_local_id(&graph_id, "graph_id").map_err(AppError::validation)?;
    let query = crate::mcp_arg_utils::mcp_arg_string(arguments, &["query", "q"])
        .ok_or_else(|| AppError::validation("query is required"))?;
    let k = arguments
        .get("k")
        .or_else(|| arguments.get("limit"))
        .and_then(Value::as_u64)
        .map_or(DEFAULT_K, |n| (n as usize).clamp(1, MAX_K));
    let mode = match crate::mcp_arg_utils::mcp_arg_string(arguments, &["mode"]).as_deref() {
        None | Some("fused") | Some("hybrid") => FindMode::Fused,
        Some("vector") | Some("meaning") => FindMode::Vector,
        Some("lexical") | Some("words") => FindMode::Lexical,
        Some(other) => {
            return Err(AppError::validation(format!(
                "mode {other:?}: use fused (default), vector or lexical"
            )))
        }
    };
    let filter = FindFilter {
        kinds: crate::mcp_arg_utils::mcp_arg_string_vec(arguments, &["kinds", "kind"]),
        types: crate::mcp_arg_utils::mcp_arg_string_vec(arguments, &["types", "type"]),
        graphs: crate::mcp_arg_utils::mcp_arg_string_vec(arguments, &["graphs", "namedGraphs"]),
    };
    let graph_dir =
        crate::paths::existing_graph_dir(&app, &graph_id).map_err(AppError::not_found)?;
    if !entity_index_enabled(&graph_dir) {
        return Err(AppError::validation(format!(
            "find is not enabled for graph {graph_id}: its owner turns it on with `gardend index --graph {graph_id}`. \
             For documents, use search_documents or semantic_search; for the RDF, sparql_query."
        )));
    }
    let index = load_entity_index(&graph_dir).map_err(|error| {
        AppError::validation(format!(
            "find is enabled for graph {graph_id} but its index is not ready ({error}); its owner builds it with `gardend index --graph {graph_id}`"
        ))
    })?;
    let started = Instant::now();
    let vector = if mode == FindMode::Lexical {
        None
    } else {
        Some(
            with_entity_embedder(&index.manifest.model_id, None, |embedder| {
                embedder.embed_query(&query)
            })
            .map_err(AppError::internal)?,
        )
    };
    let hits = index.rank(vector.as_deref(), &query, &filter, mode, k);
    Ok(find_response(
        &graph_id,
        &query,
        &index,
        &hits,
        started.elapsed().as_millis() as u64,
    ))
}

pub(crate) fn find_response(
    graph_id: &str,
    query: &str,
    index: &LoadedEntityIndex,
    hits: &[RankedHit],
    ms: u64,
) -> Value {
    // the named graph most hits come from is the response's default; a hit
    // from another one says so
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for hit in hits {
        *counts
            .entry(index.rows[hit.row].graph.as_str())
            .or_default() += 1;
    }
    let default_graph = counts
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(graph, _)| graph.to_string())
        .unwrap_or_default();
    let provenance: Vec<Value> = counts
        .keys()
        .map(|graph| {
            let mut entry = json!({ "namedGraph": graph });
            if let Some(found) = index.manifest.graphs.get(*graph) {
                for (key, value) in [
                    ("rights", &found.rights),
                    ("license", &found.license),
                    ("source", &found.source),
                ] {
                    if let Some(value) = value {
                        entry[key] = json!(value);
                    }
                }
            }
            entry
        })
        .collect();
    let hits_json: Vec<Value> = hits
        .iter()
        .map(|hit| index.hit_json(hit, &default_graph))
        .collect();
    let mut out = json!({
        "query": query,
        "graph": graph_id,
        "model": index.manifest.model_id,
        "entities": index.manifest.count,
        "hits": hits_json,
        "provenance": provenance,
        "ms": ms,
    });
    if hits.is_empty() {
        out["note"] = json!("nothing matched; try other words, either language, or fewer filters");
    } else {
        out["next"] = json!(format!(
            "every fact of a hit: sparql_query {{\"graphId\":\"{graph_id}\",\"query\":\"SELECT ?p ?o WHERE {{ GRAPH ?g {{ <IRI> ?p ?o }} }} LIMIT 50\"}} with the hit's iri as IRI; what points at it: {{ ?s ?p <IRI> }}"
        ));
    }
    out
}
