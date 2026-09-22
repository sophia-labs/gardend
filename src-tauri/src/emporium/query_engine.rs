//! The query-face EXECUTION engine — renders [`super::query_emit`]'s named
//! query plans with real graph IRIs + caller-supplied parameters and runs
//! them. The `shacl_validator`-to-`shacl_emit` split, mirrored for the query
//! face: `query_emit` derives what a class's queries LOOK like (pure);
//! this module runs them against a real store (I/O).
//!
//! **Scoping is signature-derived** ([`resolve_class_scope`] reads
//! [`crate::emporium::class_dispatch::resolve`] — `store_mode`/`store_target`
//! ONLY, never the class's name or the vocab's name), so the partitioned-store
//! silent-zero trap is structurally impossible here: every emitted query is
//! `GRAPH`-scoped to the class's OWN resolved sink(s) by construction, never a
//! bare cross-graph `SELECT`.
//!
//! **Observer semantics (RATIFIED)**: a membrane-ed class
//! (`store_target == "projection:memory"`) with no `observer` queries the
//! shared commons ONLY; a supplied `observer` widens every plan's scope to
//! commons ∪ that observer's membrane graph — recall's semantics, generalized
//! to the object query (11a-sketch). A non-membrane-ed class ignores a
//! supplied `observer` LOUDLY (a warning, never a silent no-op).
//!
//! **`currentHeads` is THE server-authoritative reader**: it runs through
//! [`crate::emporium::sweep::current_heads_by_lineage`] /
//! [`crate::emporium::sweep::heads_as_of`] — the SAME functions the `emporium_heads`
//! MCP tool and the `/emporium/heads/{graph_id}` HTTP route already call, so
//! "current head" cannot mean two different things depending on which surface
//! asks (T2 item 1).

use std::collections::BTreeMap;

use oxigraph::store::Store;
use serde_json::{json, Value};

use crate::app_runtime::AppHandle;
use crate::emporium::class_dispatch::{self, DispatchRoute};
use crate::emporium::contract::VocabularyContract;
use crate::emporium::memory_applier::open_memory_store;
use crate::emporium::objects::{class_rdf_type, ObjectError};
use crate::emporium::query_emit::{self, QueryEmitError};
use crate::emporium::{chamber_ontology::resolve_ingest_contract, sweep};
use crate::rdf::graph_subject;
use crate::rdf_authority::{
    memory_projection_graph_iri, memory_projection_graph_iri_for, user_rdf_graph_iri,
};
use crate::rdf_query_service::execute_sparql_query;

const RDF_TYPE_URI: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

fn object_error_to_query_error(error: ObjectError) -> QueryEmitError {
    let message = error.message().to_string();
    match error {
        ObjectError::BadRequest(_) => QueryEmitError::BadRequest(message),
        ObjectError::NotFound(_) | ObjectError::Absent(_) => QueryEmitError::NotFound(message),
        ObjectError::Conflict(_) => QueryEmitError::BadRequest(message),
        ObjectError::Internal(_) => QueryEmitError::Internal(message),
    }
}

/// The materialized graph(s) a class's signature resolves to for THIS
/// `graph_id` (+ optional `observer`) — the real, IRI-valued twin of
/// [`query_emit`]'s display templates.
#[derive(Debug)]
pub(crate) struct ClassScope {
    pub(crate) graphs: Vec<String>,
}

/// Resolve a class's query scope from its declared signature ONLY
/// (`class_dispatch::resolve`) — never a mode-string fork. Returns the
/// dispatch (so callers can read `store_target` etc.), the resolved scope,
/// and any structural warnings (e.g. an `observer` supplied for a class with
/// no membrane).
pub(crate) fn resolve_class_scope<'a>(
    contract: &'a VocabularyContract,
    graph_id: &str,
    class_name: &str,
    observer: Option<&str>,
) -> Result<(class_dispatch::ClassDispatch<'a>, ClassScope, Vec<String>), QueryEmitError> {
    let dispatch =
        class_dispatch::resolve(contract, class_name).map_err(QueryEmitError::BadRequest)?;
    if matches!(dispatch.route, DispatchRoute::VirtualSkip) {
        return Err(QueryEmitError::BadRequest(format!(
            "class '{class_name}' is virtual (store_mode=virtual) — nothing is materialized to \
             query here; see its declared derived_from_query"
        )));
    }
    let observer = observer.map(str::trim).filter(|o| !o.is_empty());
    let membrane_aware = dispatch.signature.store_target == "projection:memory";
    let mut warnings = Vec::new();
    let graphs = if membrane_aware {
        let commons = memory_projection_graph_iri(graph_id);
        let mut graphs = vec![commons];
        if let Some(observer) = observer {
            graphs.push(memory_projection_graph_iri_for(graph_id, observer));
        }
        graphs
    } else {
        if let Some(observer) = observer {
            warnings.push(format!(
                "class '{class_name}' (store_target='{}') has no observer membrane — observer \
                 '{observer}' ignored; queried the shared sink only",
                dispatch.signature.store_target
            ));
        }
        // The sink graph is derived from the RESOLVED ROUTE, never the raw
        // `store_target` string: `SimpleProjection`'s sink IS
        // `{graph_subject}:{store_target}` (the exact formula
        // `objects::contract_and_sink` writes to), but `WorkflowCampaign`'s
        // real sink is the ONE fixed `user:rdf` graph every class on that
        // route writes to via `apply_plan` (`rdf_authority::user_rdf_graph_iri`)
        // REGARDLESS of what string a class happens to declare as
        // `store_target` — re-splicing that string here would silently query
        // a graph nothing ever writes to for a class whose declared
        // `store_target` isn't literally `"user:rdf"`.
        match dispatch.route {
            DispatchRoute::SimpleProjection => vec![format!(
                "{}:{}",
                graph_subject(graph_id),
                dispatch.signature.store_target
            )],
            DispatchRoute::WorkflowCampaign => vec![user_rdf_graph_iri(graph_id)],
            DispatchRoute::MemorySink | DispatchRoute::VirtualSkip => {
                // Unreachable: `MemorySink` is handled by the `membrane_aware`
                // branch above, `VirtualSkip` is rejected before this
                // function reaches here.
                return Err(QueryEmitError::Internal(format!(
                    "class '{class_name}' resolved to route {:?} in the non-membrane branch — \
                     this is a query-engine bug, not a caller error",
                    dispatch.route
                )));
            }
        }
    };
    Ok((dispatch, ClassScope { graphs }, warnings))
}

/// The result of one named-query run — the rows+warnings shape
/// (`rdf_query_service::SparqlQueryResult`'s pattern), generalized: `rows` is
/// query-shaped JSON (raw `{p,o}` bindings for `byId`/`countBy`/`lineageOf`;
/// `{lineage, heads, contested}` for `currentHeads`, reusing the ratified
/// [`crate::emporium::sweep::LineageHeads`] shape directly) rather than one
/// fixed column schema across all four names.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NamedQueryOutcome {
    pub(crate) graph_id: String,
    pub(crate) vocab: String,
    pub(crate) class: String,
    pub(crate) query_name: String,
    pub(crate) variables: Vec<String>,
    pub(crate) rows: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) warnings: Vec<String>,
}

/// Run one of the four canonical named queries against a real graph.
/// `params` is query-specific: `subject` (byId/lineageOf), `asOf`
/// (currentHeads — epoch-millis or ISO datetime), `attr` (countBy).
pub(crate) fn run_named_query(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    class_name: &str,
    query_name: &str,
    observer: Option<&str>,
    params: &Value,
) -> Result<NamedQueryOutcome, QueryEmitError> {
    let store = open_memory_store(app, graph_id).map_err(QueryEmitError::Internal)?;
    let contract = resolve_ingest_contract(&store, graph_id, vocab)
        .map_err(|e| QueryEmitError::NotFound(format!("vocab '{vocab}': {e}")))?;
    let spec = contract.classes.get(class_name).cloned().ok_or_else(|| {
        QueryEmitError::NotFound(format!(
            "class '{class_name}' is not declared by vocab '{}'",
            contract.name
        ))
    })?;
    let (dispatch, scope, mut warnings) =
        resolve_class_scope(&contract, graph_id, class_name, observer)?;

    let (variables, rows, mut query_warnings) = match query_name {
        "byId" => {
            let rdf_type =
                class_rdf_type(&contract, class_name).map_err(object_error_to_query_error)?;
            run_by_id(&store, &scope, &rdf_type, params)?
        }
        "countBy" => {
            let rdf_type =
                class_rdf_type(&contract, class_name).map_err(object_error_to_query_error)?;
            run_count_by(&store, &contract, &spec, &scope, &rdf_type, params)?
        }
        "currentHeads" => run_current_heads(&store, &contract, &spec, class_name, &scope, params)?,
        "lineageOf" => run_lineage_of(&store, &contract, &spec, class_name, &scope, params)?,
        other => {
            return Err(QueryEmitError::BadRequest(format!(
            "unknown query_name '{other}' — legal values: byId, currentHeads, lineageOf, countBy"
        )))
        }
    };
    warnings.append(&mut query_warnings);
    let _ = dispatch; // resolved for its scope + virtual-skip guard only.

    Ok(NamedQueryOutcome {
        graph_id: graph_id.to_string(),
        vocab: contract.name.clone(),
        class: class_name.to_string(),
        query_name: query_name.to_string(),
        variables,
        rows,
        warnings,
    })
}

type RunOutput = (Vec<String>, Vec<Value>, Vec<String>);

fn required_str_param<'a>(
    params: &'a Value,
    key: &str,
    query_name: &str,
) -> Result<&'a str, QueryEmitError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            QueryEmitError::BadRequest(format!("{query_name} requires a non-empty '{key}' param"))
        })
}

/// Validate a caller-supplied string is safe to splice literally inside a
/// SPARQL IRI reference (`<{value}>`). The SPARQL grammar's `IRIREF` forbids
/// `<`, `>`, `"`, `{`, `}`, `|`, `^`, backslash, backtick, and any control/
/// whitespace character inside the brackets — a `subject` param carrying any
/// of these could break out of the intended `<{subject}> ?p ?o` triple
/// pattern and splice arbitrary SPARQL (a `UNION`/`GRAPH` clause reaching
/// outside the resolved class scope, e.g. into a membrane graph despite no
/// `observer`). `pub(crate)` so [`super::object_query`]'s IRI-typed criteria
/// values share this EXACT check rather than re-deriving a weaker one.
pub(crate) fn validate_sparql_iri(what: &str, value: &str) -> Result<(), QueryEmitError> {
    const FORBIDDEN: &[char] = &['<', '>', '"', '{', '}', '|', '^', '\\', '`', ' '];
    if value.is_empty()
        || value
            .chars()
            .any(|c| c.is_control() || FORBIDDEN.contains(&c))
    {
        return Err(QueryEmitError::BadRequest(format!(
            "{what} '{value}' is not a well-formed IRI reference — it must not contain \
             whitespace, control characters, or any of < > \" {{ }} | ^ \\ `"
        )));
    }
    Ok(())
}

fn run_by_id(
    store: &Store,
    scope: &ClassScope,
    rdf_type: &str,
    params: &Value,
) -> Result<RunOutput, QueryEmitError> {
    let subject = required_str_param(params, "subject", "byId")?;
    validate_sparql_iri("byId subject", subject)?;
    let sparql = format!(
        "SELECT ?p ?o WHERE {{\n  {}\n}}",
        query_emit::union_over_graphs(&scope.graphs, &format!("<{subject}> ?p ?o"))
    );
    let result = execute_sparql_query(store, &sparql).map_err(QueryEmitError::Internal)?;
    let mut warnings = result.warnings.unwrap_or_default();
    let type_triple = format!("<{RDF_TYPE_URI}>");
    let expected_type = format!("<{rdf_type}>");
    if result.rows.is_empty() {
        warnings.push(format!(
            "no triples found for <{subject}> across the resolved scope graph(s) {:?} — check \
             the subject IRI, the vocab/class, the graph_id, or (for a membrane-ed class) \
             whether the record lives under a different observer",
            scope.graphs
        ));
    } else if !result.rows.iter().any(|row| {
        row.get("p").map(String::as_str) == Some(type_triple.as_str())
            && row.get("o").map(String::as_str) == Some(expected_type.as_str())
    }) {
        warnings.push(format!(
            "<{subject}> was found in scope but carries no rdf:type <{rdf_type}> triple — it may \
             belong to a different class sharing this sink"
        ));
    }
    let rows = result
        .rows
        .into_iter()
        .map(|row| json!(row))
        .collect::<Vec<_>>();
    Ok((result.variables, rows, warnings))
}

fn run_count_by(
    store: &Store,
    contract: &VocabularyContract,
    spec: &crate::emporium::contract::ClassSpec,
    scope: &ClassScope,
    rdf_type: &str,
    params: &Value,
) -> Result<RunOutput, QueryEmitError> {
    let attr = required_str_param(params, "attr", "countBy")?;
    let attr_uri = query_emit::resolve_count_by_attr(contract, spec, attr)?;
    let body = format!("?s a <{rdf_type}> ; <{attr_uri}> ?value");
    let sparql = format!(
        "SELECT ?value (COUNT(?s) AS ?count) WHERE {{\n  {}\n}}\nGROUP BY ?value ORDER BY DESC(?count)",
        query_emit::union_over_graphs(&scope.graphs, &body)
    );
    let result = execute_sparql_query(store, &sparql).map_err(QueryEmitError::Internal)?;
    let mut warnings = result.warnings.unwrap_or_default();
    if result.rows.is_empty() {
        warnings.push(format!(
            "no '{attr}' values found for class instances in the resolved scope graph(s) {:?} — \
             either the class has no instances there yet, or none set this (optional) property",
            scope.graphs
        ));
    }
    let rows = result
        .rows
        .into_iter()
        .map(|row| json!(row))
        .collect::<Vec<_>>();
    Ok((result.variables, rows, warnings))
}

/// Format an `asOf` param (epoch-millis integer, or an ISO-8601 datetime
/// string) as a ready-to-splice `"…"^^xsd:dateTime` SPARQL literal. `pub(crate)`
/// so [`super::object_query`] (T2 item 11a: the object query's `as_of` option
/// and its `dateTime` criteria operators) shares this EXACT parsing instead of
/// re-deriving it.
///
/// A string value is LEXICALLY VALIDATED as an RFC-3339 datetime before being
/// accepted — the house's "loud rejection, never a silent zero" law applies
/// here too: `"not-a-date"` must fail loudly with a structured error, not get
/// wrapped as a bogus `xsd:dateTime` literal that then silently matches
/// nothing (or, worse, whatever the SPARQL engine happens to do with an
/// unparseable typed literal).
pub(crate) fn format_as_of_literal(value: &Value) -> Result<String, QueryEmitError> {
    let iso = match value {
        Value::Number(n) => n
            .as_i64()
            .map(crate::emporium::terms::iso_from_ms)
            .ok_or_else(|| {
                QueryEmitError::BadRequest(
                    "asOf must be an integer epoch-millis value or an ISO-8601 datetime string"
                        .to_string(),
                )
            })?,
        Value::String(s) => {
            let normalized = s.replace('Z', "+00:00");
            chrono::DateTime::parse_from_rfc3339(&normalized).map_err(|e| {
                QueryEmitError::BadRequest(format!(
                    "asOf '{s}' is not a valid ISO-8601/RFC-3339 datetime ({e}) — expected e.g. \
                     '2026-01-01T00:00:00Z' or '2026-01-01T00:00:00+00:00'"
                ))
            })?;
            s.clone()
        }
        _ => {
            return Err(QueryEmitError::BadRequest(
                "asOf must be an integer epoch-millis value or an ISO-8601 datetime string"
                    .to_string(),
            ))
        }
    };
    let escaped = iso.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        "\"{escaped}\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"
    ))
}

fn heads_rows(heads_by_lineage: BTreeMap<String, Vec<String>>) -> Vec<Value> {
    heads_by_lineage
        .into_iter()
        .map(|(lineage, heads)| {
            let contested = heads.len() > 1;
            json!({ "lineage": lineage, "heads": heads, "contested": contested })
        })
        .collect()
}

fn run_current_heads(
    store: &Store,
    contract: &VocabularyContract,
    spec: &crate::emporium::contract::ClassSpec,
    class_name: &str,
    scope: &ClassScope,
    params: &Value,
) -> Result<RunOutput, QueryEmitError> {
    let shape = query_emit::lineage_shape(contract, spec).ok_or_else(|| {
        QueryEmitError::BadRequest(format!(
            "class '{class_name}' declares no `lineage`/`isCurrent` predicate pair — \
             currentHeads is unsupported for this class"
        ))
    })?;
    let variables = vec![
        "lineage".to_string(),
        "heads".to_string(),
        "contested".to_string(),
    ];
    let mut warnings = Vec::new();

    let heads_by_lineage = match params.get("asOf") {
        Some(as_of_value) if !as_of_value.is_null() => {
            let created_at = shape.created_at.clone().ok_or_else(|| {
                QueryEmitError::BadRequest(format!(
                    "class '{class_name}' declares no `createdAt` predicate — `asOf` is unavailable"
                ))
            })?;
            let supersedes = shape.supersedes.clone().ok_or_else(|| {
                QueryEmitError::BadRequest(format!(
                    "class '{class_name}' declares no `supersedes` predicate — `asOf` is unavailable"
                ))
            })?;
            let literal = format_as_of_literal(as_of_value)?;
            sweep::heads_as_of(
                store,
                &scope.graphs,
                &shape.lineage,
                &created_at,
                &supersedes,
                &literal,
            )
            .map_err(QueryEmitError::Internal)?
        }
        _ => {
            sweep::current_heads_by_lineage(store, &scope.graphs, &shape.lineage, &shape.is_current)
                .map_err(QueryEmitError::Internal)?
        }
    };
    if heads_by_lineage.is_empty() {
        warnings.push(format!(
            "no current heads found for class '{class_name}' in the resolved scope graph(s) \
             {:?} — either the class has no instances yet in scope, or (for a membrane-ed \
             class) the relevant observer was not supplied",
            scope.graphs
        ));
    }
    Ok((variables, heads_rows(heads_by_lineage), warnings))
}

fn run_lineage_of(
    store: &Store,
    contract: &VocabularyContract,
    spec: &crate::emporium::contract::ClassSpec,
    class_name: &str,
    scope: &ClassScope,
    params: &Value,
) -> Result<RunOutput, QueryEmitError> {
    let shape = query_emit::lineage_shape(contract, spec).ok_or_else(|| {
        QueryEmitError::BadRequest(format!(
            "class '{class_name}' declares no `lineage`/`isCurrent` predicate pair"
        ))
    })?;
    let supersedes = shape.supersedes.ok_or_else(|| {
        QueryEmitError::BadRequest(format!(
            "class '{class_name}' declares no `supersedes` predicate — there is no chain to walk"
        ))
    })?;
    let subject = required_str_param(params, "subject", "lineageOf")?;
    validate_sparql_iri("lineageOf subject", subject)?;
    let body = format!("<{subject}> (<{supersedes}>|^<{supersedes}>)* ?node");
    let sparql = format!(
        "SELECT DISTINCT ?node WHERE {{\n  {}\n}}",
        query_emit::union_over_graphs(&scope.graphs, &body)
    );
    let result = execute_sparql_query(store, &sparql).map_err(QueryEmitError::Internal)?;
    let mut warnings = result.warnings.unwrap_or_default();
    if result.rows.len() <= 1 {
        warnings.push(format!(
            "the chain from <{subject}> has no other members in scope — either it was never \
             superseded/contradicted, or the sibling versions live under a different observer's \
             membrane"
        ));
    }
    let rows = result
        .rows
        .into_iter()
        .map(|row| json!(row))
        .collect::<Vec<_>>();
    Ok((result.variables, rows, warnings))
}

#[cfg(test)]
mod pure_tests {
    use super::*;
    use crate::emporium::contract::memory_core_vocabulary;

    #[test]
    fn as_of_literal_from_epoch_millis_and_iso_string_agree() {
        let from_ms = format_as_of_literal(&json!(1_750_000_000_000i64)).unwrap();
        let from_iso = format_as_of_literal(&json!("2025-06-15T15:06:40+00:00")).unwrap();
        assert_eq!(from_ms, from_iso);
        assert!(from_ms.ends_with("^^<http://www.w3.org/2001/XMLSchema#dateTime>"));
    }

    #[test]
    fn as_of_literal_rejects_non_temporal_values() {
        assert!(format_as_of_literal(&json!(true)).is_err());
        assert!(format_as_of_literal(&json!([1, 2])).is_err());
    }

    #[test]
    fn as_of_literal_rejects_a_malformed_datetime_string_loudly() {
        let err = format_as_of_literal(&json!("not-a-date")).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.message().contains("not-a-date"), "{}", err.message());
    }

    #[test]
    fn validate_sparql_iri_accepts_ordinary_subjects_and_rejects_injection_payloads() {
        assert!(validate_sparql_iri("subject", "urn:sophia:memory:record:abc123").is_ok());
        assert!(validate_sparql_iri("subject", "https://example.test/thing").is_ok());

        // The exact BLOCKING scenario the review named: a `subject` param
        // carrying a closing `>` plus SPARQL syntax to break out of the
        // intended `<{subject}> ?p ?o` triple pattern and splice a UNION/
        // GRAPH clause reaching outside the resolved class scope.
        let injection =
            "urn:sophia:x> } UNION { GRAPH <urn:mnemosyne:local:graph:lab:projection:memory:agent:secret> { ?s ?p ?o";
        let err = validate_sparql_iri("subject", injection).unwrap_err();
        assert_eq!(err.status(), 400);

        assert!(
            validate_sparql_iri("subject", "").is_err(),
            "empty is rejected"
        );
        assert!(validate_sparql_iri("subject", "has space").is_err());
        assert!(validate_sparql_iri("subject", "has\nnewline").is_err());
        assert!(validate_sparql_iri("subject", "has\"quote").is_err());
    }

    #[test]
    fn heads_rows_flags_contested_lineages() {
        let mut map = BTreeMap::new();
        map.insert("urn:l1".to_string(), vec!["urn:h1".to_string()]);
        map.insert(
            "urn:l2".to_string(),
            vec!["urn:h2a".to_string(), "urn:h2b".to_string()],
        );
        let rows = heads_rows(map);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["contested"], json!(false));
        assert_eq!(rows[1]["contested"], json!(true));
    }

    #[test]
    fn unknown_query_name_is_a_structured_bad_request() {
        // Exercises the dispatch arm without a store: resolve_class_scope alone
        // (no store needed) proves the virtual-skip guard; the full dispatch is
        // covered end-to-end under the headless real-store tests below.
        let contract = memory_core_vocabulary();
        let (_dispatch, _scope, warnings) =
            resolve_class_scope(contract, "lab", "MemoryRecord", None).expect("resolves");
        assert!(warnings.is_empty(), "no observer supplied ⇒ no warning");
    }

    #[test]
    fn non_membrane_class_warns_when_observer_supplied() {
        let contract = crate::emporium::contract::get_vocabulary("emporium-bookmark").unwrap();
        let (_dispatch, scope, warnings) =
            resolve_class_scope(contract, "lab", "Bookmark", Some("agent-x")).expect("resolves");
        assert_eq!(scope.graphs.len(), 1, "no membrane ⇒ one sink graph");
        assert_eq!(warnings.len(), 1, "observer-ignored warning fires");
        assert!(warnings[0].contains("agent-x"));
    }

    #[test]
    fn virtual_class_is_rejected_before_any_query_runs() {
        let contract = crate::emporium::contract::get_vocabulary("workflow").unwrap();
        let err = resolve_class_scope(contract, "lab", "Draft", None).unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.message().contains("virtual"));
    }
}

#[cfg(all(test, feature = "headless"))]
mod headless_tests {
    use super::*;
    use crate::emporium::planner::memory_record_subject;
    use crate::emporium::schemas::{MemoryRecordIn, SourceRefIn};
    use crate::geist_memory_service::ingest_memory_record;
    use crate::graph_service::{create_graph_service, CreateGraphInput};

    fn temp_profile(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-query-engine-{name}-{nanos}"))
    }

    fn record(client_ref: &str, content: &str, supersedes: Option<String>) -> MemoryRecordIn {
        MemoryRecordIn {
            client_ref: Some(client_ref.to_string()),
            scope: "agent".to_string(),
            kind: "ClaimMemory".to_string(),
            content_orientation: "knowledge".to_string(),
            visibility: "private".to_string(),
            status: "active".to_string(),
            content: content.to_string(),
            source_refs: vec![SourceRefIn {
                source_kind: "DocumentBlock".to_string(),
                source_label: None,
                block_id: Some("abc".to_string()),
                document_id: Some("doc-shell".to_string()),
                external_id: None,
                external_uri: None,
                observed_at: None,
                trust_tier: None,
            }],
            evidence: vec![],
            observed_at: Some(1_718_700_000_000),
            valid_from: Some(1_718_700_000_000),
            is_current: Some(true),
            confidence: None,
            valence: None,
            agent_id: Some("gamma".to_string()),
            observer_agent_id: None,
            tags: vec![],
            supersedes_ref: supersedes,
            contradicts_ref: None,
        }
    }

    fn record_at(
        client_ref: &str,
        content: &str,
        supersedes: Option<String>,
        observed_at_ms: i64,
    ) -> MemoryRecordIn {
        let mut r = record(client_ref, content, supersedes);
        r.observed_at = Some(observed_at_ms);
        r.valid_from = Some(observed_at_ms);
        r
    }

    fn observed_record(client_ref: &str, content: &str, observer: &str) -> MemoryRecordIn {
        let mut r = record(client_ref, content, None);
        // I2 (voice containment): a record carrying BOTH a voice leaf
        // (`mem:agentId`) and a witness (`mem:observedBy`) must have the leaf
        // name an `agt:Voice` of THAT witness — `record()`'s default
        // `agent_id: Some("gamma")` has no such relation to a synthetic
        // observer, so clear it here (this fixture tests observer scoping,
        // not the voice-containment invariant).
        r.agent_id = None;
        r.observer_agent_id = Some(observer.to_string());
        r
    }

    fn run_isolated(name: &str, body: impl FnOnce() + std::panic::UnwindSafe) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile(name);
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(body);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn by_id_reads_the_seeded_bookmark_and_warns_on_a_missing_subject() {
        run_isolated("byid", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "query-engine-byid";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Query Engine ByID".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            crate::app_runtime::async_runtime::block_on(crate::emporium::objects::create_objects(
                &app,
                graph_id,
                "emporium-bookmark",
                serde_json::json!([{
                    "kind": "Bookmark",
                    "localId": "qe-book",
                    "url": "https://example.test/qe",
                    "title": "QE Book",
                }]),
            ))
            .expect("seed bookmark");

            let subject_read = crate::emporium::objects::read_object(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                "qe-book",
            )
            .expect("read seeded subject");
            let subject = subject_read["subject"].as_str().unwrap().to_string();

            let outcome = run_named_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                "byId",
                None,
                &serde_json::json!({ "subject": subject }),
            )
            .expect("byId runs");
            assert!(
                !outcome.rows.is_empty(),
                "{outcome:?}",
                outcome = outcome.rows
            );
            assert!(
                outcome.warnings.is_empty(),
                "found subject ⇒ no warning: {:?}",
                outcome.warnings
            );

            let missing = run_named_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                "byId",
                None,
                &serde_json::json!({ "subject": "urn:sophia:no-such-subject" }),
            )
            .expect("byId runs even on a miss");
            assert!(missing.rows.is_empty());
            assert_eq!(
                missing.warnings.len(),
                1,
                "the silent zero is flagged loudly"
            );
            assert!(
                missing.warnings[0].contains("no such subject")
                    || missing.warnings[0].contains("no triples found")
            );
        });
    }

    /// The BLOCKING scenario the review named, exercised through the FULL
    /// `run_named_query` call (not just the pure validator): a `subject`
    /// param carrying `>` plus SPARQL syntax must be a structured rejection,
    /// never silently spliced into `<{subject}> ?p ?o` where it could break
    /// out into a `UNION`/`GRAPH` clause reaching outside the resolved scope.
    #[test]
    fn by_id_rejects_a_subject_param_carrying_sparql_injection_syntax() {
        run_isolated("byid-injection", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "query-engine-byid-injection";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Query Engine ByID Injection".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            let injected_subject =
                "urn:sophia:x> } UNION { GRAPH <urn:mnemosyne:local:graph:query-engine-byid-injection:projection:memory:agent:secret> { ?s ?p ?o";
            let err = run_named_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                "byId",
                None,
                &serde_json::json!({ "subject": injected_subject }),
            )
            .unwrap_err();
            assert_eq!(err.status(), 400, "{}", err.message());

            let lineage_err = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "lineageOf",
                None,
                &serde_json::json!({ "subject": injected_subject }),
            )
            .unwrap_err();
            assert_eq!(lineage_err.status(), 400, "{}", lineage_err.message());
        });
    }

    #[test]
    fn count_by_rejects_an_undeclared_attr_and_counts_a_declared_one() {
        run_isolated("countby", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "query-engine-countby";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Query Engine CountBy".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            for (id, title) in [("b1", "Alpha"), ("b2", "Beta")] {
                crate::app_runtime::async_runtime::block_on(crate::emporium::objects::create_objects(
                    &app,
                    graph_id,
                    "emporium-bookmark",
                    serde_json::json!([{
                        "kind": "Bookmark",
                        "localId": id,
                        "url": format!("https://example.test/{id}"),
                        "title": title,
                    }]),
                ))
                .expect("seed bookmark");
            }

            let rejected = run_named_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                "countBy",
                None,
                &serde_json::json!({ "attr": "notAField" }),
            )
            .unwrap_err();
            assert_eq!(rejected.status(), 400);
            assert!(rejected.message().contains("legal values"));

            let counted = run_named_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                "countBy",
                None,
                &serde_json::json!({ "attr": "url" }),
            )
            .expect("countBy runs on a declared attr");
            assert_eq!(
                counted.rows.len(),
                2,
                "two distinct urls, one bookmark each: {:?}",
                counted.rows
            );
        });
    }

    #[test]
    fn current_heads_and_lineage_of_surface_a_real_contested_fork() {
        run_isolated("heads", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "query-engine-heads";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Query Engine Heads".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            let h = record("qe-h", "vera prefers fish CLI", None);
            let h_subject = memory_record_subject(graph_id, &h);
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, h))
                .expect("file H");
            let a = record("qe-a", "vera prefers zsh", Some(h_subject.clone()));
            let b = record("qe-b", "vera prefers nushell", Some(h_subject.clone()));
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, a))
                .expect("file A (supersedes H)");
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, b))
                .expect("file B (also supersedes H)");

            let heads = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                None,
                &serde_json::json!({}),
            )
            .expect("currentHeads runs");
            let contested_row = heads
                .rows
                .iter()
                .find(|row| row["contested"] == serde_json::json!(true))
                .expect("a contested lineage is present");
            assert_eq!(
                contested_row["heads"].as_array().map(Vec::len),
                Some(2),
                "{contested_row:?}"
            );

            let chain = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "lineageOf",
                None,
                &serde_json::json!({ "subject": h_subject }),
            )
            .expect("lineageOf runs");
            assert_eq!(
                chain.rows.len(),
                3,
                "H plus its two superseding siblings: {:?}",
                chain.rows
            );
        });
    }

    #[test]
    fn as_of_reconstructs_a_prior_head_after_a_later_supersession() {
        run_isolated("asof", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "query-engine-asof";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Query Engine AsOf".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            let t0 = 1_718_700_000_000i64;
            let t1 = t0 + 3_600_000; // an hour later

            let h = record_at("qe-asof-h", "vera prefers fish CLI", None, t0);
            let h_subject = memory_record_subject(graph_id, &h);
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, h))
                .expect("file H at t0");

            let a = record_at(
                "qe-asof-a",
                "vera prefers zsh now",
                Some(h_subject.clone()),
                t1,
            );
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, a))
                .expect("file A (supersedes H) at t1");

            // Live: H is superseded, A is the sole current head.
            let live = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                None,
                &serde_json::json!({}),
            )
            .expect("live currentHeads runs");
            let live_row = live
                .rows
                .iter()
                .find(|row| row["lineage"] == serde_json::json!(h_subject))
                .expect("lineage present live");
            assert_eq!(live_row["heads"].as_array().map(Vec::len), Some(1));
            assert_ne!(
                live_row["heads"][0],
                serde_json::json!(h_subject),
                "{live_row:?}"
            );

            // asOf a moment BEFORE A existed: H is (still) the head.
            let historical = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                None,
                &serde_json::json!({ "asOf": t0 + 60_000 }),
            )
            .expect("asOf currentHeads runs");
            let historical_row = historical
                .rows
                .iter()
                .find(|row| row["lineage"] == serde_json::json!(h_subject))
                .expect("lineage present historically");
            assert_eq!(
                historical_row["heads"],
                serde_json::json!([h_subject]),
                "{historical_row:?}"
            );
        });
    }

    /// The exact scenario the sweep.rs review finding named: H lives in the
    /// shared COMMONS; a witness later supersedes it with A filed in THAT
    /// witness's own membrane graph, a DIFFERENT graph than H. An
    /// observer-scoped `asOf` after A must see the supersession — H must NOT
    /// resurrect as a second "current" head just because the commons branch
    /// of the scoped union can't see into the membrane branch.
    #[test]
    fn as_of_sees_a_cross_graph_supersession_when_observer_is_supplied() {
        run_isolated("asof-cross-graph", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "query-engine-asof-cross-graph";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Query Engine AsOf Cross Graph".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            let t0 = 1_718_700_000_000i64;
            let t1 = t0 + 3_600_000; // an hour later

            // H is filed in the shared COMMONS (no observer).
            let h = record_at("qe-xg-h", "vera prefers fish CLI", None, t0);
            let h_subject = memory_record_subject(graph_id, &h);
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, h))
                .expect("file H in commons at t0");

            // A supersedes H but is filed in a WITNESS's OWN membrane, not
            // commons (I2 voice containment: clear the default agent_id when
            // setting observer_agent_id, same discipline `observed_record`
            // uses elsewhere in this module).
            let mut a = record_at(
                "qe-xg-a",
                "vera prefers zsh now",
                Some(h_subject.clone()),
                t1,
            );
            a.agent_id = None;
            a.observer_agent_id = Some("agent-xg-witness".to_string());
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, a))
                .expect("file A (supersedes H, in the witness membrane) at t1");

            // asOf AFTER A, WITH the observer supplied (commons ∪ that
            // membrane): A must be the SOLE head.
            let after = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                Some("agent-xg-witness"),
                &serde_json::json!({ "asOf": t1 + 60_000 }),
            )
            .expect("asOf with observer runs");
            let row = after
                .rows
                .iter()
                .find(|row| row["lineage"] == serde_json::json!(h_subject))
                .expect("lineage present");
            assert_eq!(
                row["heads"].as_array().map(Vec::len),
                Some(1),
                "H must be recognized as superseded even though A lives in a DIFFERENT \
                 (membrane) graph than H (commons) — {row:?}"
            );
            assert_ne!(row["heads"][0], serde_json::json!(h_subject), "{row:?}");
        });
    }

    /// The sweep.rs review finding: `heads_as_of` used to exclude ANY
    /// candidate carrying a retraction predicate, with no comparison against
    /// `asOf` — so a record retracted "now" (real wall-clock time) would
    /// vanish even from a historical read of a moment LONG BEFORE that
    /// retraction happened. A retraction is a timestamped event like any
    /// other; an `asOf` query strictly before it must still see the head.
    #[test]
    fn as_of_before_a_later_retraction_still_sees_the_head() {
        run_isolated("asof-retract", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "query-engine-asof-retract";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Query Engine AsOf Retract".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            // Filed long in the past — "now" (the retraction's real
            // wall-clock timestamp, below) is guaranteed to be later.
            let t0 = 1_718_700_000_000i64;
            let h = record_at("qe-asof-retract-h", "vera prefers fish CLI", None, t0);
            let h_subject = memory_record_subject(graph_id, &h);
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, h))
                .expect("file H at t0");

            // Live, pre-retraction: H is the sole current head.
            let live_before = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                None,
                &serde_json::json!({}),
            )
            .expect("live currentHeads runs");
            let row_before = live_before
                .rows
                .iter()
                .find(|row| row["lineage"] == serde_json::json!(h_subject))
                .expect("lineage present before retraction");
            assert_eq!(row_before["heads"], serde_json::json!([h_subject]));

            // Retract H now (real wall-clock `retractedAt`, long after t0).
            crate::app_runtime::async_runtime::block_on(crate::emporium::write::emporium_retract(
                &app,
                graph_id,
                &h_subject,
                "test: superseded by nothing, just retiring the note",
                "retract",
                None,
            ))
            .expect("retract H");

            // Live, post-retraction: H is excluded — the lineage produces no
            // row at all (a retracted-only lineage has zero current heads).
            let live_after = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                None,
                &serde_json::json!({}),
            )
            .expect("live currentHeads (post-retraction) runs");
            assert!(
                !live_after
                    .rows
                    .iter()
                    .any(|row| row["lineage"] == serde_json::json!(h_subject)),
                "H must be excluded live, post-retraction: {live_after:?}"
            );

            // asOf shortly after t0 — LONG BEFORE the retraction above ran —
            // must still see H as the head: the retraction postdates this
            // instant, so it must not hide a head from before it happened.
            let historical = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                None,
                &serde_json::json!({ "asOf": t0 + 60_000 }),
            )
            .expect("asOf currentHeads runs");
            let historical_row = historical
                .rows
                .iter()
                .find(|row| row["lineage"] == serde_json::json!(h_subject))
                .expect(
                    "lineage present historically — a later retraction must not hide a head \
                     from before it happened",
                );
            assert_eq!(
                historical_row["heads"],
                serde_json::json!([h_subject]),
                "{historical_row:?}"
            );
        });
    }

    #[test]
    fn observer_split_no_observer_sees_commons_only() {
        run_isolated("observer", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "query-engine-observer";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Query Engine Observer".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            let commons = record("qe-obs-commons", "commons memory: vera likes tea", None);
            let commons_outcome =
                crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, commons))
                    .expect("file commons memory");
            assert_eq!(
                commons_outcome["ok"],
                serde_json::json!(true),
                "{commons_outcome}"
            );
            let witnessed = observed_record(
                "qe-obs-witness",
                "witness-only memory: a secret",
                "agent-witness-x",
            );
            let witnessed_subject = memory_record_subject(graph_id, &witnessed);
            let witnessed_outcome =
                crate::app_runtime::async_runtime::block_on(ingest_memory_record(&app, graph_id, witnessed))
                    .expect("file witnessed memory");
            assert_eq!(
                witnessed_outcome["ok"],
                serde_json::json!(true),
                "{witnessed_outcome}"
            );

            // No observer: byId sees the commons record …
            let commons_read = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "currentHeads",
                None,
                &serde_json::json!({}),
            )
            .expect("commons currentHeads runs");
            assert!(
                commons_read.rows.len() == 1,
                "commons-only read must NOT see the witnessed lineage: {:?}",
                commons_read.rows
            );

            // …but NOT the witness-only one — byId on it warns "not found" (commons ∪ nothing).
            let miss = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "byId",
                None,
                &serde_json::json!({ "subject": witnessed_subject.clone() }),
            )
            .expect("byId runs even on a commons miss");
            assert!(
                miss.rows.is_empty(),
                "commons-only scope must miss the witness-only record"
            );
            assert_eq!(miss.warnings.len(), 1);

            // With the observer supplied: commons ∪ that observer's membrane — the
            // witnessed record is now reachable.
            let hit = run_named_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                "byId",
                Some("agent-witness-x"),
                &serde_json::json!({ "subject": witnessed_subject }),
            )
            .expect("byId runs with the observer supplied");
            assert!(
                !hit.rows.is_empty(),
                "commons ∪ observer membrane must reach the witnessed record"
            );
        });
    }
}
