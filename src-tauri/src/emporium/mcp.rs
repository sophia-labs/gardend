//! MCP handlers for the emporium vocabulary registry + the violation ledger —
//! the cheapest proof of the "trampoline into an in-cell Rust engine" pattern
//! (Phase 1), plus the one T0 reader (`emporium_violations`) that needs direct
//! store access and so lives here rather than the top-level
//! `emporium_mcp_surface` (which only calls emporium's already-`pub(crate)`
//! surface — `open_memory_store` is `pub(super)`, scoped to this module tree).
//!
//! `emporium_vocab` returns a registered vocabulary contract. With no `name`
//! it returns the catalog (the same `{name,version,namespace,sha,title}` rows
//! the `/emporium/vocabs` HTTP endpoint serves). With a `name` (and optional
//! `version`, default `latest`) it returns that contract: its metadata plus the
//! canonical JSON body (parsed, so MCP clients get structured content) and the
//! sha used as the cross-stack identity/ETag.
//!
//! `emporium_violations` is a read-only SPARQL listing of the violation ledger
//! (`:projection:violations`, EA-6) — the observer-relative testimony both the
//! write-time SHACL gate (`FlagAndAccept`) and the §8.1 conformance sweep file
//! there (see `violation_ledger.rs` for the shape each record carries).

use crate::app_error::{AppError, AppResult};
use crate::app_runtime::AppHandle;
use crate::emporium::chamber_ontology::{
    list_chamber_ontologies, resolve_chamber_ontology_version,
};
use crate::emporium::contract::{Datatype, VocabularyContract};
use crate::emporium::memory_applier::open_memory_store;
use crate::emporium::terms::{sha256_text, term_for, Value as TermValue};
use crate::emporium::type_catalog::class_catalog;
use crate::emporium::violation_ledger::VLOG_NS;
use crate::emporium::vocabs::{all_contracts, find_contract, VocabContract};
use crate::mcp_arg_utils::{mcp_arg_string, mcp_arg_usize, mcp_required_graph_id};
use crate::rdf_authority::violations_projection_graph_iri;
use oxigraph::model::Term;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use serde_json::Value;

/// Local handler for the `emporium_vocab` MCP tool. Reads `name` (camelCase or
/// snake_case via `mcp_arg_string`), an optional `version` (default `latest`)
/// and an optional `graphId`. Returns the catalog when `name` is absent, the
/// resolved contract when present, or a not-found error when the name/version
/// is unknown.
///
/// With a `graphId` the graph's own types answer too (D3, 2026-10-20): the
/// catalog lists its domain ontologies (active and superseded, `source:
/// "domain"`) after the built-ins, and a name the built-ins do not have
/// resolves to the graph's domain ontology of that name (embedded wins, as
/// `resolve_ingest_contract` does): `latest` is the active version, any other
/// version that exact one. Every named read carries `classCatalog`, one entry
/// per class with its derived JSON Schema (a built-in's label falls back to
/// the class name).
pub(crate) fn mcp_local_emporium_vocab(app: &AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_arg_string(arguments, &["graphId", "graph_id"]);
    match graph_id {
        Some(graph_id) => {
            let store = open_memory_store(app, &graph_id).map_err(AppError::not_found)?;
            emporium_vocab_payload(arguments, Some((&store, &graph_id)))
        }
        None => emporium_vocab_payload(arguments, None),
    }
}

/// [`mcp_local_emporium_vocab`] over an already-open graph store (or none).
pub(crate) fn emporium_vocab_payload(
    arguments: &Value,
    graph: Option<(&oxigraph::store::Store, &str)>,
) -> AppResult<Value> {
    let name = mcp_arg_string(arguments, &["name", "vocab", "vocabulary"]);
    let version = mcp_arg_string(arguments, &["version"]).unwrap_or_else(|| "latest".to_string());

    let Some(name) = name else {
        return catalog_payload(graph);
    };

    if let Some(contract) = find_contract(&name, &version) {
        return contract_payload(&contract);
    }
    if let Some((store, graph_id)) = graph {
        if let Some(payload) = domain_payload(store, graph_id, &name, &version)? {
            return Ok(payload);
        }
    }
    Err(AppError::not_found(format!(
        "emporium vocabulary not found: name={name} version={version}{}",
        if graph.is_some() {
            ""
        } else {
            " (a graph's own types are found with graphId)"
        }
    )))
}

/// The catalog view — every registered vocabulary's summary row, then (with
/// a graph) each of the graph's domain ontology versions, each with its own
/// `classCatalog`, and a top-level `classCatalog` of the graph's types: every
/// class of its ACTIVE domain ontologies (the built-ins' classes are read by
/// name, so the listing stays small).
pub(crate) fn catalog_payload(graph: Option<(&oxigraph::store::Store, &str)>) -> AppResult<Value> {
    let mut vocabularies: Vec<Value> = all_contracts().iter().map(contract_summary).collect();
    let Some((store, graph_id)) = graph else {
        return Ok(serde_json::json!({ "vocabularies": vocabularies }));
    };
    let mut graph_types: Vec<Value> = Vec::new();
    for record in list_chamber_ontologies(store, graph_id).map_err(AppError::internal)? {
        let parsed: Option<VocabularyContract> = serde_json::from_str(&record.contract_json).ok();
        let catalog = parsed
            .as_ref()
            .map(|c| class_catalog(c, &record.version))
            .unwrap_or_default();
        if record.status == "active" {
            graph_types.extend(catalog.iter().cloned());
        }
        vocabularies.push(serde_json::json!({
            "name": record.name,
            "version": record.version,
            "namespace": parsed.as_ref().map(|c| c.primary_namespace().to_string()),
            "sha": sha256_text(&record.contract_json),
            "title": parsed.as_ref().map(|c| c.title.clone()),
            "source": "domain",
            "status": record.status,
            "ontologySubject": record.subject,
            "classes": parsed.as_ref().map(|c| c.classes.keys().cloned().collect::<Vec<_>>()),
            "classCatalog": catalog,
        }));
    }
    Ok(serde_json::json!({ "vocabularies": vocabularies, "classCatalog": graph_types }))
}

/// A single vocabulary summary row (matches the HTTP `/emporium/vocabs` shape,
/// plus `source: "builtin"`).
fn contract_summary(contract: &VocabContract) -> Value {
    serde_json::json!({
        "name": contract.name,
        "version": contract.version,
        "namespace": contract.namespace,
        "sha": contract.sha,
        "title": contract.title,
        "publicJurisdiction": contract.public_jurisdiction,
        "canonicalOntology": contract.canonical_ontology,
        "canonicalPack": contract.canonical_pack,
        "registryStatus": contract.registry_status,
        "slugAliases": contract.slug_aliases,
        "compatibilityAliases": contract.compatibility_aliases,
        "source": "builtin",
    })
}

/// The full contract payload: summary metadata + the canonical JSON body as
/// structured content + the class catalogue. The body is parsed (not
/// re-serialized for the wire) so MCP clients receive structure; the sha
/// still pins the canonical bytes.
pub(crate) fn contract_payload(contract: &VocabContract) -> AppResult<Value> {
    let contract_json: Value = serde_json::from_str(contract.json).map_err(|error| {
        AppError::serialization(format!("parse embedded vocab contract: {error}"))
    })?;
    let class_catalog = crate::emporium::contract::get_vocabulary(contract.name)
        .map(|parsed| class_catalog(parsed, contract.version))
        .unwrap_or_default();
    Ok(serde_json::json!({
        "name": contract.name,
        "version": contract.version,
        "namespace": contract.namespace,
        "sha": contract.sha,
        "title": contract.title,
        "publicJurisdiction": contract.public_jurisdiction,
        "canonicalOntology": contract.canonical_ontology,
        "canonicalPack": contract.canonical_pack,
        "registryStatus": contract.registry_status,
        "slugAliases": contract.slug_aliases,
        "compatibilityAliases": contract.compatibility_aliases,
        "source": "builtin",
        "contract": contract_json,
        "classCatalog": class_catalog,
    }))
}

/// A graph's domain ontology `name` at `version` (`latest` = active), in the
/// same envelope as a built-in, with `source: "domain"`, its lifecycle and
/// every version of that name. `Ok(None)` when the graph has none.
pub(crate) fn domain_payload(
    store: &oxigraph::store::Store,
    graph_id: &str,
    name: &str,
    version: &str,
) -> AppResult<Option<Value>> {
    let Some(resolved) = resolve_chamber_ontology_version(store, graph_id, name, version)
        .map_err(AppError::internal)?
    else {
        return Ok(None);
    };
    let records = list_chamber_ontologies(store, graph_id).map_err(AppError::internal)?;
    let this = records.iter().find(|r| r.subject == resolved.subject);
    let versions: Vec<Value> = records
        .iter()
        .filter(|r| r.name == name)
        .map(|r| serde_json::json!({ "version": r.version, "status": r.status, "ontologySubject": r.subject }))
        .collect();
    let contract_json: Value = serde_json::from_str(&resolved.contract_json).map_err(|error| {
        AppError::serialization(format!("parse domain ontology contract: {error}"))
    })?;
    let contract = &resolved.contract;
    Ok(Some(serde_json::json!({
        "name": contract.name,
        "version": resolved.version,
        "namespace": contract.primary_namespace(),
        "sha": sha256_text(&resolved.contract_json),
        "title": contract.title,
        "source": "domain",
        "status": this.map(|r| r.status.clone()),
        "ontologySubject": resolved.subject,
        "supersedes": this.and_then(|r| r.supersedes.clone()),
        "supersededBy": this.and_then(|r| r.superseded_by.clone()),
        "versions": versions,
        "contract": contract_json,
        "classCatalog": class_catalog(contract, &resolved.version),
    })))
}

/// Local handler for the `emporium_violations` MCP tool: a read-only SPARQL
/// listing of the violation ledger (`:projection:violations`). Required
/// `graph_id`/`graphId`; optional `severity` (e.g. "Violation"/"Warning"),
/// `observer` (the witness URI that filed the finding — the write-time
/// `shacl-validator` or the sweep's `conformance-sweep`), and `limit` (default
/// 100, clamped to 1..=500). Newest first (`observedAt` descending). Returns
/// `{graphId, ledger, limit, violations:[{subject, focusNode, message,
/// severity, observedAt, observer, shape?, propertyPath?, offendingValue?}]}`.
pub(crate) fn mcp_local_emporium_violations(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let severity = mcp_arg_string(arguments, &["severity"]);
    let observer = mcp_arg_string(arguments, &["observer"]);
    let limit = mcp_arg_usize(arguments, &["limit"], 100).clamp(1, 500);

    let store = open_memory_store(&app, &graph_id).map_err(AppError::internal)?;
    let ledger_graph = violations_projection_graph_iri(&graph_id);

    // Optional filters, rendered as safely-escaped SPARQL literal/URI terms via
    // the SAME `term_for` mint the ledger writer uses (byte-consistent compare).
    let mut filters = String::new();
    if let Some(severity) = &severity {
        let literal = term_for(&TermValue::Str(severity.clone()), Datatype::string).as_nt();
        filters.push_str(&format!(" FILTER(?severity = {literal})"));
    }
    if let Some(observer) = &observer {
        let uri = term_for(&TermValue::Uri(observer.clone()), Datatype::uri).as_nt();
        filters.push_str(&format!(" FILTER(?observer = {uri})"));
    }

    let query = format!(
        "SELECT ?v ?focus ?message ?severity ?observedAt ?observer ?shape ?path ?value WHERE {{ \
         GRAPH <{ledger_graph}> {{ \
           ?v a <{VLOG_NS}Violation> ; \
              <{VLOG_NS}focusNode> ?focus ; \
              <{VLOG_NS}message> ?message ; \
              <{VLOG_NS}severity> ?severity ; \
              <{VLOG_NS}observedAt> ?observedAt ; \
              <http://www.w3.org/ns/prov#wasAttributedTo> ?observer . \
           OPTIONAL {{ ?v <{VLOG_NS}shape> ?shape }} \
           OPTIONAL {{ ?v <{VLOG_NS}path> ?path }} \
           OPTIONAL {{ ?v <{VLOG_NS}offendingValue> ?value }} \
         }}{filters} }} ORDER BY DESC(?observedAt) LIMIT {limit}"
    );

    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|error| AppError::internal(format!("parse violations query: {error}")))?
        .on_store(&store)
        .execute()
        .map_err(|error| AppError::internal(format!("execute violations query: {error}")))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => {
            return Err(AppError::internal(
                "violations query expected SELECT solutions",
            ))
        }
    };

    // Plain lexical values (not raw N-Triples) — the MCP-friendly rendering;
    // `literal`/`uri` are Copy closures (no captures), reused across bindings.
    let literal = |term: &Term| -> String {
        match term {
            Term::Literal(value) => value.value().to_string(),
            other => other.to_string(),
        }
    };
    let uri = |term: &Term| -> String {
        match term {
            Term::NamedNode(name) => name.as_str().to_string(),
            other => other.to_string(),
        }
    };

    let mut violations = Vec::new();
    for solution in solutions {
        let solution =
            solution.map_err(|error| AppError::internal(format!("violations row: {error}")))?;
        violations.push(serde_json::json!({
            "subject": solution.get("v").map(uri).unwrap_or_default(),
            "focusNode": solution.get("focus").map(literal).unwrap_or_default(),
            "message": solution.get("message").map(literal).unwrap_or_default(),
            "severity": solution.get("severity").map(literal).unwrap_or_default(),
            "observedAt": solution.get("observedAt").map(literal).unwrap_or_default(),
            "observer": solution.get("observer").map(uri).unwrap_or_default(),
            "shape": solution.get("shape").map(literal),
            "propertyPath": solution.get("path").map(literal),
            "offendingValue": solution.get("value").map(literal),
        }));
    }

    Ok(serde_json::json!({
        "graphId": graph_id,
        "ledger": ledger_graph,
        "limit": limit,
        "violations": violations,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emporium_vocab_without_name_returns_catalog() {
        let result = emporium_vocab_payload(&serde_json::json!({}), None).unwrap();
        let vocabularies = result
            .get("vocabularies")
            .and_then(Value::as_array)
            .expect("catalog has vocabularies array");
        // The catalog serves every registered pack off the table-driven
        // `all_contracts()` — a registry ROW lights up the MCP surface with no
        // further wiring.
        assert_eq!(vocabularies.len(), 23);
        let names: Vec<&str> = vocabularies
            .iter()
            .filter_map(|v| v.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"workflow"));
        assert!(names.contains(&"sophia-memory-core"));
        assert!(names.contains(&"emporium-bookmark"));
        assert!(names.contains(&"koch-morse"));
        assert!(names.contains(&"sophia-api"));
        assert!(names.contains(&"emporium-chamber"));
        assert!(names.contains(&"sophia-agent-core"));
        assert!(names.contains(&"wf-agent-binding"));
        assert!(names.contains(&"wf-agent-session-projection"));
        assert!(names.contains(&"wf-agent-world-runtime"));
        assert!(names.contains(&"kg-ultra-intuition"));
        assert!(names.contains(&"lme-labeled-memory"));
        assert!(names.contains(&"workflow-ui"));
        assert!(names.contains(&"lex-scotus-core"));
        assert!(names.contains(&"sophia-machine-core"));
        assert!(names.contains(&"emporium-observatory"));
        assert!(names.contains(&"sophia-domain-manifest"));
        assert!(names.contains(&"sophia-domain-verdict"));
        assert!(names.contains(&"sophia-domain-dashboard"));
        assert!(names.contains(&"shrubbery-site"));
        assert!(names.contains(&"flow"));
        assert!(names.contains(&"garden-file-views"));
        let wf = vocabularies
            .iter()
            .find(|v| v.get("name").and_then(Value::as_str) == Some("workflow"))
            .expect("workflow in catalog");
        assert_eq!(
            wf.get("sha").and_then(Value::as_str),
            Some(crate::emporium::vocabs::WORKFLOW_GOLDEN_SHA)
        );
        assert_eq!(
            wf.get("publicJurisdiction").and_then(Value::as_str),
            Some("workflow")
        );
        assert_eq!(
            wf.get("registryStatus").and_then(Value::as_str),
            Some("canonical-public")
        );
        let mem = vocabularies
            .iter()
            .find(|v| v.get("name").and_then(Value::as_str) == Some("sophia-memory-core"))
            .expect("memory pack in catalog");
        assert_eq!(
            mem.get("sha").and_then(Value::as_str),
            Some(crate::emporium::vocabs::MEMORY_CORE_GOLDEN_SHA)
        );
        assert_eq!(
            mem.get("slugAliases")
                .and_then(Value::as_array)
                .and_then(|aliases| aliases.first())
                .and_then(Value::as_str),
            Some("memory")
        );
    }

    #[test]
    fn emporium_vocab_returns_named_contract_with_body() {
        let result =
            emporium_vocab_payload(&serde_json::json!({ "name": "workflow" }), None).unwrap();
        assert_eq!(result.get("name").and_then(Value::as_str), Some("workflow"));
        assert_eq!(
            result.get("sha").and_then(Value::as_str),
            Some(crate::emporium::vocabs::WORKFLOW_GOLDEN_SHA)
        );
        // The structured contract body parsed back to an object.
        let contract = result.get("contract").expect("contract body present");
        assert_eq!(
            contract.get("name").and_then(Value::as_str),
            Some("workflow")
        );
        assert_eq!(
            contract.get("primary_prefix").and_then(Value::as_str),
            Some("wf")
        );
    }

    #[test]
    fn emporium_vocab_accepts_snake_and_camel_version() {
        assert!(emporium_vocab_payload(
            &serde_json::json!({ "name": "workflow", "version": "1.0.0" }),
            None
        )
        .is_ok());
    }

    #[test]
    fn emporium_vocab_unknown_name_is_not_found() {
        let err = emporium_vocab_payload(&serde_json::json!({ "name": "nope" }), None).unwrap_err();
        assert_eq!(err.kind(), crate::app_error::AppErrorKind::NotFound);
    }
}

// `emporium_violations` needs a REAL per-graph oxigraph store (`open_memory_store`
// resolves an on-disk graph dir), so this suite runs under the headless harness —
// mirrors the `build_mock_app_for_tests` + temp-profile pattern used throughout
// `emporium/state_trace_tests.rs` / `geist_memory_backfill.rs`. NO MOCKS: real
// store, real `append_violations` ledger writer, real SPARQL read.
#[cfg(all(test, feature = "headless"))]
mod violations_tests {
    use super::*;
    use crate::emporium::shacl_validator::ViolationRecord;
    use crate::emporium::violation_ledger::append_violations;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn env_serial() -> &'static Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-emporium-mcp-violations-{name}-{nanos}"))
    }

    fn mock_app() -> AppHandle {
        crate::tauri_runtime::build_mock_app_for_tests(true)
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Violations Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    fn violation(focus: &str, severity: &str) -> ViolationRecord {
        ViolationRecord {
            focus_node: focus.to_string(),
            shape: Some("urn:sophia:shacl:sophia-memory-core#SourceReferenceShape".to_string()),
            property_path: None,
            offending_value: None,
            message: format!("test violation for {focus}"),
            severity: severity.to_string(),
        }
    }

    #[test]
    fn emporium_violations_reads_filters_and_limits_the_ledger() {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile("read");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(run_emporium_violations_trace);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn run_emporium_violations_trace() {
        let app = mock_app();
        let graph_id = "emporium-mcp-violations-lab";
        seed_graph(&app, graph_id);

        let store = open_memory_store(&app, graph_id).expect("open memory store");
        let now = chrono::Utc::now().timestamp_millis();
        append_violations(
            &store,
            graph_id,
            "urn:sophia:observer:shacl-validator",
            now,
            &[violation("urn:test:a", "Violation")],
        )
        .expect("file violation A");
        append_violations(
            &store,
            graph_id,
            "urn:sophia:observer:conformance-sweep",
            now + 1,
            &[violation("urn:test:b", "Warning")],
        )
        .expect("file violation B");

        // No filter: both rows come back, newest (observedAt desc) first.
        let all = mcp_local_emporium_violations(
            app.clone(),
            &serde_json::json!({ "graph_id": graph_id }),
        )
        .expect("emporium_violations reads the ledger");
        let rows = all["violations"].as_array().expect("violations array");
        assert_eq!(rows.len(), 2, "{all}");
        assert_eq!(
            rows[0]["focusNode"],
            serde_json::json!("urn:test:b"),
            "newest first: {all}"
        );

        // severity filter.
        let warnings = mcp_local_emporium_violations(
            app.clone(),
            &serde_json::json!({ "graph_id": graph_id, "severity": "Warning" }),
        )
        .expect("severity-filtered read");
        let rows = warnings["violations"].as_array().expect("violations array");
        assert_eq!(rows.len(), 1, "{warnings}");
        assert_eq!(rows[0]["focusNode"], serde_json::json!("urn:test:b"));

        // observer filter.
        let by_sweep = mcp_local_emporium_violations(
            app.clone(),
            &serde_json::json!({
                "graph_id": graph_id,
                "observer": "urn:sophia:observer:conformance-sweep",
            }),
        )
        .expect("observer-filtered read");
        let rows = by_sweep["violations"].as_array().expect("violations array");
        assert_eq!(rows.len(), 1, "{by_sweep}");
        assert_eq!(
            rows[0]["observer"],
            serde_json::json!("urn:sophia:observer:conformance-sweep")
        );

        // limit.
        let limited = mcp_local_emporium_violations(
            app.clone(),
            &serde_json::json!({ "graph_id": graph_id, "limit": 1 }),
        )
        .expect("limited read");
        assert_eq!(limited["violations"].as_array().map(Vec::len), Some(1));
        assert_eq!(limited["limit"], serde_json::json!(1));

        // graph_id is required.
        let missing_graph = mcp_local_emporium_violations(app, &serde_json::json!({}));
        assert!(missing_graph.is_err(), "graph_id is required");
    }
}
