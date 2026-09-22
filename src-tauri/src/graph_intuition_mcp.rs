use crate::{
    app_error::{AppError, AppResult},
    app_runtime::AppHandle,
    local_service_host::{LocalServiceState, KG_ULTRA_SERVICE_ID},
    loopback_state::LoopbackState,
    mcp_arg_utils::{mcp_arg_bool, mcp_arg_string, mcp_arg_usize, mcp_required_graph_id},
    omphalos::{read_oracle_constitution, OracleBinding, OracleConstitution, OracleTaskSelection},
    rdf_authority::kg_ultra_projection_graph_iri,
    runtime_config::PROFILE_ID,
};
use reqwest::Method;
use serde_json::{json, Value};
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;

const KG_ULTRA_INTUITION_PATH: &str = "/api/intuition";

#[derive(Debug, Clone)]
struct GraphIntuitionPlan {
    task_id: String,
    task_kind: String,
    query_shape: String,
    top_k: usize,
    min_score: f64,
    max_hops: usize,
    warnings: Vec<String>,
    raw_sparql: String,
    body: Value,
}

pub(super) async fn mcp_local_graph_intuition(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let execute = mcp_arg_bool(arguments, &["execute"], false);
    let auto_start = mcp_arg_bool(arguments, &["autoStart", "auto_start"], false);
    let oracle_kind = mcp_arg_string(arguments, &["oracleKind", "oracle_kind"])
        .unwrap_or_else(|| "kg-ultra".to_string());
    let oracle =
        read_oracle_constitution(&app, &graph_id, &oracle_kind).map_err(AppError::validation)?;
    let Some(oracle) = oracle else {
        return Ok(unavailable_book(
            &graph_id,
            &oracle_kind,
            "oracle_unavailable",
            "No enabled Omphalos OracleBinding matched this graph.",
        ));
    };

    if !oracle.oracle.enabled {
        return Ok(unavailable_book(
            &graph_id,
            &oracle_kind,
            "oracle_disabled",
            "The Omphalos oracle exists but is disabled.",
        ));
    }
    if oracle.binding.mode.trim().to_ascii_lowercase() != "active" {
        return Ok(blocked_book(
            &graph_id,
            &oracle,
            "binding_inactive",
            &format!(
                "OracleBinding {} is mode '{}', not active.",
                oracle.binding.subject, oracle.binding.mode
            ),
        ));
    }

    let plan = match build_graph_intuition_plan(&graph_id, arguments, &oracle) {
        Ok(plan) => plan,
        Err(book) => return Ok(book),
    };
    let execution = if execute {
        Some(execute_graph_intuition(app, &plan, auto_start).await)
    } else {
        None
    };

    Ok(render_graph_intuition_book(
        &graph_id, &oracle, &plan, execution,
    ))
}

fn build_graph_intuition_plan(
    graph_id: &str,
    arguments: &Value,
    oracle: &OracleConstitution,
) -> Result<GraphIntuitionPlan, Value> {
    let query_shape = match infer_query_shape(arguments) {
        Ok(shape) => shape,
        Err(message) => {
            return Err(blocked_book(
                graph_id,
                oracle,
                "query_shape_required",
                &message,
            ))
        }
    };
    let task_kind = if is_logical_shape(&query_shape) {
        "logical-query-answering"
    } else {
        "link-prediction"
    };
    let Some(task) = oracle
        .tasks
        .iter()
        .find(|task| task.task_kind == task_kind || task.task_id == task_kind)
    else {
        return Err(blocked_book(
            graph_id,
            oracle,
            "task_unavailable",
            &format!("The {task_kind} task is not declared for this oracle."),
        ));
    };
    if !oracle.binding.allowed_tasks.is_empty()
        && !oracle.binding.allowed_tasks.contains(&task.task_id)
    {
        return Err(blocked_book(
            graph_id,
            oracle,
            "task_not_allowed",
            &format!(
                "OracleBinding {} does not allow task {}.",
                oracle.binding.subject, task.task_id
            ),
        ));
    }
    if let Err(message) = validate_query_shape(arguments, &query_shape, &oracle.binding, task) {
        return Err(blocked_book(
            graph_id,
            oracle,
            "query_not_allowed",
            &message,
        ));
    }

    let (top_k, min_score, max_hops, warnings) = broker_knobs(arguments, &oracle.binding);
    let raw_sparql = recent_intuitions_sparql(graph_id);
    let query = json!({
        "shape": query_shape,
        "headIri": string_arg(arguments, &["headIri", "head_iri"]),
        "tailIri": string_arg(arguments, &["tailIri", "tail_iri"]),
        "relationIri": string_arg(arguments, &["relationIri", "relation_iri", "predicateIri", "predicate"]),
        "queryAst": arguments.get("queryAst").or_else(|| arguments.get("query_ast")).cloned(),
        "query": string_arg(arguments, &["query"]),
    });
    let body = json!({
        "graphId": graph_id,
        "oracleKind": oracle.oracle.oracle_kind,
        "taskId": task.task_id,
        "taskKind": task.task_kind,
        "modelId": task.model_id,
        "modelVersion": task.model_version,
        "query": query,
        "options": {
            "topK": top_k,
            "minScore": min_score,
            "maxHops": max_hops,
            "allowPath": oracle.binding.allow_path,
            "allowIntersection": oracle.binding.allow_intersection,
            "allowUnion": oracle.binding.allow_union,
            "allowNegation": oracle.binding.allow_negation,
            "weights": {
                "structural": oracle.binding.structural_weight,
                "embedding": oracle.binding.embedding_weight,
                "lexical": oracle.binding.lexical_weight,
                "normalized": oracle.binding.weights_normalized,
            },
        },
        "output": {
            "vocab": oracle.oracle.output_vocab,
            "vocabVersion": oracle.oracle.output_vocab_version,
            "projectionTarget": oracle.oracle.projection_target,
            "persistPolicy": oracle.binding.persist_policy,
            "requiresAcceptance": oracle.binding.requires_acceptance,
            "maxPersistedCandidates": oracle.binding.max_persisted_candidates,
        },
    });

    Ok(GraphIntuitionPlan {
        task_id: task.task_id.clone(),
        task_kind: task.task_kind.clone(),
        query_shape,
        top_k,
        min_score,
        max_hops,
        warnings,
        raw_sparql,
        body,
    })
}

fn infer_query_shape(arguments: &Value) -> Result<String, String> {
    let explicit = string_arg(
        arguments,
        &[
            "queryShape",
            "query_shape",
            "queryKind",
            "query_kind",
            "kind",
        ],
    )
    .or_else(|| {
        arguments
            .get("queryAst")
            .or_else(|| arguments.get("query_ast"))
            .and_then(|ast| string_arg(ast, &["kind", "shape"]))
    });
    if let Some(shape) = explicit {
        return normalize_query_shape(&shape);
    }
    let head = string_arg(arguments, &["headIri", "head_iri"]).is_some();
    let tail = string_arg(arguments, &["tailIri", "tail_iri"]).is_some();
    let relation = string_arg(
        arguments,
        &["relationIri", "relation_iri", "predicateIri", "predicate"],
    )
    .is_some();
    match (head, tail, relation) {
        (true, false, true) => Ok("link".to_string()),
        (false, true, true) => Ok("reverseLink".to_string()),
        (true, true, true) => Ok("link".to_string()),
        _ => Err(
            "graph_intuition needs either a link-prediction triple shape or queryAst/queryShape for logical answering."
                .to_string(),
        ),
    }
}

fn normalize_query_shape(shape: &str) -> Result<String, String> {
    match shape.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "link" | "link-prediction" | "predict-tail" | "score-triple" => Ok("link".to_string()),
        "reverse-link" | "reverse-link-prediction" | "predict-head" => {
            Ok("reverseLink".to_string())
        }
        "path" | "path-query" => Ok("path".to_string()),
        "intersection" | "and" | "conjunction" => Ok("intersection".to_string()),
        "union" | "or" | "disjunction" => Ok("union".to_string()),
        "logical" | "logicalquery" | "logical-query" | "logical-query-answering" => {
            Ok("logicalQuery".to_string())
        }
        other => Err(format!("unsupported graph_intuition query shape '{other}'")),
    }
}

fn validate_query_shape(
    arguments: &Value,
    query_shape: &str,
    binding: &OracleBinding,
    task: &OracleTaskSelection,
) -> Result<(), String> {
    match query_shape {
        "path" if !binding.allow_path || !task.supports_path => {
            Err("path queries are disabled by Omphalos for this graph".to_string())
        }
        "intersection" if !binding.allow_intersection || !task.supports_intersection => {
            Err("intersection queries are disabled by Omphalos for this graph".to_string())
        }
        "union" if !binding.allow_union || !task.supports_union => {
            Err("union queries are disabled by Omphalos for this graph".to_string())
        }
        _ => {
            let ast = arguments
                .get("queryAst")
                .or_else(|| arguments.get("query_ast"));
            if ast.is_some_and(contains_negation)
                && (!binding.allow_negation || !task.supports_negation)
            {
                return Err(
                    "negated logical clauses are disabled by Omphalos for this graph".to_string(),
                );
            }
            Ok(())
        }
    }
}

fn broker_knobs(arguments: &Value, binding: &OracleBinding) -> (usize, f64, usize, Vec<String>) {
    let mut warnings = Vec::new();
    let requested_top_k = mcp_arg_usize(arguments, &["topK", "top_k", "limit"], binding.top_k);
    let top_k = requested_top_k.clamp(1, binding.top_k.max(1));
    if top_k != requested_top_k {
        warnings.push(format!(
            "topK clamped from {requested_top_k} to Omphalos limit {top_k}"
        ));
    }
    let requested_min_score = arg_f64(arguments, &["minScore", "min_score"], binding.min_score);
    let min_score = requested_min_score.max(binding.min_score);
    if (min_score - requested_min_score).abs() > f64::EPSILON {
        warnings.push(format!(
            "minScore raised from {requested_min_score} to Omphalos floor {min_score}"
        ));
    }
    let requested_max_hops = mcp_arg_usize(arguments, &["maxHops", "max_hops"], binding.max_hops);
    let max_hops = requested_max_hops.clamp(1, binding.max_hops.max(1));
    if max_hops != requested_max_hops {
        warnings.push(format!(
            "maxHops clamped from {requested_max_hops} to Omphalos limit {max_hops}"
        ));
    }
    (top_k, min_score, max_hops, warnings)
}

async fn execute_graph_intuition(
    app: AppHandle,
    plan: &GraphIntuitionPlan,
    auto_start: bool,
) -> Value {
    let state = match local_loopback_state(&app) {
        Ok(state) => state,
        Err(error) => {
            return json!({
                "status": "loopback_unavailable",
                "error": error.to_string(),
            })
        }
    };
    let mut service_status = state.services.status(KG_ULTRA_SERVICE_ID, &state.app);
    if !matches!(service_status.state, LocalServiceState::Running) && auto_start {
        match state.services.start(
            KG_ULTRA_SERVICE_ID,
            &state.app,
            &state.manifest,
            &state.token,
        ) {
            Ok(status) => service_status = status,
            Err(error) => {
                return json!({
                    "status": "service_unavailable",
                    "serviceStatus": service_status,
                    "error": error,
                })
            }
        }
    }
    if !matches!(service_status.state, LocalServiceState::Running) {
        return json!({
            "status": "service_not_running",
            "serviceStatus": service_status,
            "message": "Call graph_intuition with execute=true and autoStart=true, or start kg-ultra first.",
        });
    }
    match kg_ultra_json_request(&state, Method::POST, KG_ULTRA_INTUITION_PATH, &plan.body).await {
        Ok(upstream) => json!({
            "status": if upstream.status.is_success() { "ok" } else { "upstream_error" },
            "httpStatus": upstream.status.as_u16(),
            "body": upstream.body,
        }),
        Err(error) => json!({
            "status": "upstream_error",
            "error": error.to_string(),
        }),
    }
}

fn render_graph_intuition_book(
    graph_id: &str,
    oracle: &OracleConstitution,
    plan: &GraphIntuitionPlan,
    execution: Option<Value>,
) -> Value {
    let executed = execution.is_some();
    let response_ok = execution
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        == Some("ok");
    let recommended = if response_ok {
        json!({
            "routeId": "inspectRawSparql",
            "label": "Inspect persisted oracle testimony",
            "tool": "sparql_query",
            "arguments": {"graphId": graph_id, "query": plan.raw_sparql},
        })
    } else {
        json!({
            "routeId": "execute",
            "label": "Run graph intuition",
            "tool": "graph_intuition",
            "arguments": execution_arguments(graph_id, &plan.body),
        })
    };
    json!({
        "kind": "graphIntuitionResult",
        "status": if response_ok { "ok" } else if executed { "execution_pending_or_unavailable" } else { "ready" },
        "graphId": graph_id,
        "oracle": oracle_summary(oracle),
        "selection": {
            "taskId": plan.task_id,
            "taskKind": plan.task_kind,
            "queryShape": plan.query_shape,
            "topK": plan.top_k,
            "minScore": plan.min_score,
            "maxHops": plan.max_hops,
            "warnings": plan.warnings,
        },
        "request": {
            "facade": {"method": "POST", "path": "/api/kg-ultra/intuition"},
            "alias": {"method": "POST", "path": "/api/kg-ultra/graph-intuition"},
            "upstream": {"serviceId": "kg-ultra", "method": "POST", "path": KG_ULTRA_INTUITION_PATH},
            "body": plan.body,
        },
        "response": execution,
        "book": {
            "kind": "intuitionStorybook",
            "title": "Graph Intuition",
            "pages": [
                {
                    "id": "oracle",
                    "title": "Oracle",
                    "summary": format!("{} via {}", oracle.oracle.oracle_kind, oracle.oracle.service_id),
                    "status": if oracle.oracle.enabled { "enabled" } else { "disabled" },
                },
                {
                    "id": "request",
                    "title": "Compiled Request",
                    "summary": format!("{} / {}", plan.task_kind, plan.query_shape),
                    "body": plan.body,
                },
                {
                    "id": "authority",
                    "title": "Authority",
                    "summary": "Oracle candidates are suggestions; accepting a relation uses create_wires.",
                }
            ],
            "routes": [
                recommended,
                {
                    "routeId": "inspectRawSparql",
                    "label": "Inspect persisted oracle testimony",
                    "tool": "sparql_query",
                    "arguments": {"graphId": graph_id, "query": plan.raw_sparql},
                },
                {
                    "routeId": "acceptCandidate",
                    "label": "Accept candidate as a Garden wire",
                    "tool": "create_wires",
                    "argumentsTemplate": {
                        "graphId": graph_id,
                        "wires": [{"from": "<candidate head>", "predicate": "<candidate relation>", "to": "<candidate tail>"}],
                    },
                }
            ],
            "recommendedNextAction": if response_ok { "inspectRawSparql" } else { "execute" },
        },
        "rawSparql": plan.raw_sparql,
    })
}

fn unavailable_book(graph_id: &str, oracle_kind: &str, status: &str, message: &str) -> Value {
    json!({
        "kind": "graphIntuitionResult",
        "status": status,
        "graphId": graph_id,
        "oracle": {"oracleKind": oracle_kind, "available": false},
        "book": {
            "kind": "intuitionStorybook",
            "title": "Graph Intuition",
            "pages": [{"id": "unavailable", "title": "Unavailable", "summary": message}],
            "routes": [],
            "recommendedNextAction": null,
        },
        "message": message,
    })
}

fn blocked_book(graph_id: &str, oracle: &OracleConstitution, status: &str, message: &str) -> Value {
    json!({
        "kind": "graphIntuitionResult",
        "status": status,
        "graphId": graph_id,
        "oracle": oracle_summary(oracle),
        "book": {
            "kind": "intuitionStorybook",
            "title": "Graph Intuition",
            "pages": [{"id": "blocked", "title": "Blocked", "summary": message}],
            "routes": [],
            "recommendedNextAction": null,
        },
        "message": message,
    })
}

fn oracle_summary(oracle: &OracleConstitution) -> Value {
    json!({
        "oracleKind": oracle.oracle.oracle_kind,
        "serviceId": oracle.oracle.service_id,
        "enabled": oracle.oracle.enabled,
        "inputLayer": oracle.oracle.input_layer,
        "outputVocab": oracle.oracle.output_vocab,
        "outputVocabVersion": oracle.oracle.output_vocab_version,
        "projectionTarget": oracle.oracle.projection_target,
        "binding": {
            "graphId": oracle.binding.graph_id,
            "mode": oracle.binding.mode,
            "allowedTasks": oracle.binding.allowed_tasks,
            "requiresAcceptance": oracle.binding.requires_acceptance,
        },
    })
}

fn execution_arguments(graph_id: &str, body: &Value) -> Value {
    let query = body.get("query").cloned().unwrap_or_else(|| json!({}));
    json!({
        "graphId": graph_id,
        "execute": true,
        "queryShape": query.get("shape").cloned(),
        "headIri": query.get("headIri").cloned(),
        "tailIri": query.get("tailIri").cloned(),
        "relationIri": query.get("relationIri").cloned(),
        "queryAst": query.get("queryAst").cloned(),
    })
}

fn recent_intuitions_sparql(graph_id: &str) -> String {
    let projection = kg_ultra_projection_graph_iri(graph_id);
    format!(
        "PREFIX kgultra: <http://mnemosyne.dev/kg-ultra#>\nPREFIX prov: <http://www.w3.org/ns/prov#>\nSELECT ?intuition ?taskKind ?candidate ?answer ?rank ?score WHERE {{\n  GRAPH <{projection}> {{\n    ?intuition a kgultra:Intuition .\n    OPTIONAL {{ ?intuition kgultra:taskKind ?taskKind }}\n    OPTIONAL {{ ?intuition kgultra:hasCandidate ?candidate . ?candidate kgultra:rank ?rank . OPTIONAL {{ ?candidate kgultra:score ?score }} }}\n    OPTIONAL {{ ?intuition kgultra:hasAnswerCandidate ?answer . ?answer kgultra:rank ?rank . OPTIONAL {{ ?answer kgultra:score ?score }} }}\n  }}\n}}\nORDER BY DESC(?intuition) ?rank\nLIMIT 50"
    )
}

fn is_logical_shape(shape: &str) -> bool {
    matches!(shape, "path" | "intersection" | "union" | "logicalQuery")
}

fn contains_negation(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(
                key.trim().to_ascii_lowercase().as_str(),
                "not" | "negation" | "negate" | "minus"
            ) || contains_negation(value)
        }),
        Value::Array(values) => values.iter().any(contains_negation),
        _ => false,
    }
}

fn string_arg(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn arg_f64(arguments: &Value, keys: &[&str], default: f64) -> f64 {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_f64))
        .filter(|value| value.is_finite())
        .unwrap_or(default)
}

fn local_loopback_state(app: &AppHandle) -> AppResult<Arc<LoopbackState>> {
    app.try_state::<Arc<LoopbackState>>()
        .map(|state| state.inner().clone())
        .ok_or_else(|| {
            AppError::validation(
                "local loopback state is unavailable; graph intuition execution requires local mode",
            )
        })
}

struct UpstreamJson {
    status: reqwest::StatusCode,
    body: Value,
}

async fn kg_ultra_json_request(
    state: &Arc<LoopbackState>,
    method: Method,
    path: &str,
    body: &Value,
) -> AppResult<UpstreamJson> {
    let target = state
        .services
        .target(KG_ULTRA_SERVICE_ID)
        .map_err(AppError::validation)?;
    let url = build_upstream_url(&target.base_url, path);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| AppError::internal(format!("build KG-ULTRA client: {error}")))?;
    let response = client
        .request(method, &url)
        .header("Accept", "application/json")
        .header("X-Internal-Service", target.internal_secret)
        .header("X-User-ID", PROFILE_ID)
        .json(body)
        .send()
        .await
        .map_err(|error| AppError::internal(format!("call KG-ULTRA service: {error}")))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| AppError::internal(format!("read KG-ULTRA response: {error}")))?;
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        json!({
            "text": String::from_utf8_lossy(&bytes).to_string(),
        })
    });
    Ok(UpstreamJson { status, body })
}

fn build_upstream_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::omphalos::{OracleBinding, OracleSelection};

    fn oracle() -> OracleConstitution {
        OracleConstitution {
            oracle: OracleSelection {
                subject: "urn:oracle".to_string(),
                oracle_kind: "kg-ultra".to_string(),
                service_id: "kg-ultra".to_string(),
                enabled: true,
                input_layer: "rdf-graph".to_string(),
                output_vocab: "kg-ultra-intuition".to_string(),
                output_vocab_version: Some("1.1.0".to_string()),
                projection_target: "projection:kg-ultra".to_string(),
            },
            tasks: vec![
                OracleTaskSelection {
                    subject: "urn:task:link".to_string(),
                    task_id: "link-prediction".to_string(),
                    task_kind: "link-prediction".to_string(),
                    model_id: "ultra_4g".to_string(),
                    model_version: None,
                    supports_path: false,
                    supports_intersection: false,
                    supports_union: false,
                    supports_negation: false,
                    default_top_k: 50,
                    default_min_score: 0.2,
                },
                OracleTaskSelection {
                    subject: "urn:task:logical".to_string(),
                    task_id: "logical-query-answering".to_string(),
                    task_kind: "logical-query-answering".to_string(),
                    model_id: "ultraquery_4g".to_string(),
                    model_version: None,
                    supports_path: true,
                    supports_intersection: true,
                    supports_union: true,
                    supports_negation: false,
                    default_top_k: 50,
                    default_min_score: 0.2,
                },
            ],
            binding: OracleBinding {
                subject: "urn:binding".to_string(),
                graph_id: "*".to_string(),
                mode: "active".to_string(),
                allowed_tasks: vec![
                    "link-prediction".to_string(),
                    "logical-query-answering".to_string(),
                ],
                top_k: 25,
                min_score: 0.2,
                max_hops: 3,
                allow_path: true,
                allow_intersection: true,
                allow_union: true,
                allow_negation: false,
                structural_weight: 0.55,
                embedding_weight: 0.35,
                lexical_weight: 0.10,
                weights_normalized: false,
                persist_policy: "on-request".to_string(),
                requires_acceptance: true,
                max_persisted_candidates: 10,
            },
        }
    }

    #[test]
    fn graph_intuition_compiles_link_prediction_request() {
        let args = json!({
            "graphId": "lab",
            "headIri": "urn:a",
            "relationIri": "http://example.test/rel",
            "topK": 100,
        });
        let plan = build_graph_intuition_plan("lab", &args, &oracle()).expect("plan");
        assert_eq!(plan.task_kind, "link-prediction");
        assert_eq!(plan.query_shape, "link");
        assert_eq!(plan.top_k, 25);
        assert_eq!(plan.body["query"]["headIri"], "urn:a");
        assert_eq!(plan.body["options"]["weights"]["structural"], 0.55);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("topK clamped")));
    }

    #[test]
    fn graph_intuition_compiles_logical_query_request() {
        let args = json!({
            "graphId": "lab",
            "queryShape": "path",
            "queryAst": {"kind": "path", "from": "urn:a", "predicate": "urn:rel"},
            "maxHops": 9,
        });
        let plan = build_graph_intuition_plan("lab", &args, &oracle()).expect("plan");
        assert_eq!(plan.task_kind, "logical-query-answering");
        assert_eq!(plan.query_shape, "path");
        assert_eq!(plan.max_hops, 3);
        assert_eq!(plan.body["query"]["queryAst"]["kind"], "path");
    }

    #[test]
    fn graph_intuition_blocks_negation_when_binding_disallows_it() {
        let args = json!({
            "graphId": "lab",
            "queryShape": "path",
            "queryAst": {"kind": "path", "not": {"predicate": "urn:rel"}},
        });
        let blocked = build_graph_intuition_plan("lab", &args, &oracle()).unwrap_err();
        assert_eq!(blocked["status"], "query_not_allowed");
        assert!(blocked["message"]
            .as_str()
            .unwrap()
            .contains("negated logical clauses"));
    }

    #[test]
    fn graph_intuition_accepts_schema_logical_query_spelling() {
        let args = json!({
            "graphId": "lab",
            "queryShape": "logicalQuery",
            "queryAst": {"kind": "logicalQuery", "not": {"predicate": "urn:rel"}},
        });
        let blocked = build_graph_intuition_plan("lab", &args, &oracle()).unwrap_err();
        assert_eq!(blocked["status"], "query_not_allowed");
        assert!(blocked["message"]
            .as_str()
            .unwrap()
            .contains("negated logical clauses"));
    }
}
