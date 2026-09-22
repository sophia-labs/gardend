//! Vocab → OpenAPI emitter — the OpenAPI FACE of a Meaningful-Objects API
//! publication face (EA-3 Seq 9 / UC-2).
//!
//! This is the second public materializer and a PEER of [`super::shacl_emit::vocab_to_shacl`]:
//! just as the SHACL face is a deterministic projection of the contract's class /
//! predicate signatures, the OpenAPI face is a deterministic projection of the
//! PUBLISHED workflow invocation surface — the concrete canonical
//! `wf:Operation` / `wf:Parameter` / `wf:Response` / `wf:WorkflowBinding` /
//! `wf:Server` instances, with legacy `api:` aliases accepted during migration —
//! into a valid OpenAPI 3.x document.
//!
//! ## One source, many faces
//!
//! The Workflow vocab (the [`VocabularyContract`]) is the canonical source. It is
//! consumed two ways:
//!   - its CLASS signatures → SHACL shapes ([`super::shacl_emit::vocab_to_shacl`]) —
//!     the validation face for the published instances;
//!   - its published INSTANCES (the born-RDF in `:projection:api`) → an OpenAPI 3.x
//!     document (this module) — the API face.
//!
//! The contract supplies the document's metadata (title / version / description)
//! and the namespace bindings; the [`ApiSurface`] supplies the operations. In the
//! LIVE path the surface is read from the graph via SPARQL by the caller; this
//! materializer is PURE over `(contract, surface)` — no I/O, no store, deterministic
//! (`serde_json` is built with `preserve_order`, so insertion order is the wire
//! order).
//!
//! ## The CA-1 layer split — JSON Schema passes through VERBATIM
//!
//! Per the CA-1 invocation-binding contract, an [`ApiBinding`]'s `input_schema` /
//! `output_schema` are JSON-Schema **string literals** (JSON content carried in an
//! RDF string literal). Emporium does NOT translate between the contract datatype
//! alphabet and JSON Schema: the materializer PARSES the literal (validating it is
//! syntactically JSON — a loud halt otherwise) and embeds the parsed value
//! UNCHANGED into `components/schemas`. The contract's bare datatype tokens stay in
//! the SHACL face; the workflow's JSON Schema stays in the OpenAPI face. Two
//! alphabets, two layers, no collision.
//!
//! ## The mapping
//!
//! | published wf: triple               | OpenAPI                                      |
//! |------------------------------------|----------------------------------------------|
//! | `wf:Operation` (id/method/path)    | `paths/{path}/{method}` PathItem + operation |
//! | `wf:Parameter` (name/in/datatype)  | an entry in the operation `parameters` array |
//! | `wf:WorkflowBinding.inputSchema`   | `requestBody` schema (`$ref` into components) |
//! | `wf:WorkflowBinding.outputSchema`  | the `200` response schema (`$ref`)           |
//! | `wf:WorkflowBinding.workflowName`  | `x-workflow-name` extension (discovery hook) |
//! | `wf:WorkflowBinding.executor`      | `x-executor` extension (discovery hook)      |
//! | `wf:Response` (statusCode/desc)    | an entry in the operation `responses` map    |
//! | `wf:Server` (url/description)      | an entry in the top-level `servers` array    |
//!
//! ## Execution boundary
//!
//! This module remains a pure publication materializer. The loopback service's
//! live Operation route resolves the exact published face with
//! [`read_api_operation`], pins its workflow source digest, and then sends a
//! canonical invocation envelope to Choreograph. Publication and execution thus
//! share one graph-authored binding without putting I/O in the materializer.

use std::collections::BTreeMap;

use oxigraph::model::Term;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use serde_json::{json, Map, Value};

use crate::emporium::contract::VocabularyContract;
use crate::rdf::graph_subject;

/// One published API parameter (canonical `wf:Parameter`, legacy `api:Parameter`
/// during migration). `datatype` is a bare contract token
/// (`string`/`integer`/`boolean`/…); the materializer maps it to the OpenAPI
/// primitive schema type.
#[derive(Debug, Clone)]
pub(crate) struct ApiParameter {
    pub(crate) name: String,
    /// `query` | `path` | `header` | `cookie` (the OpenAPI `in` field).
    pub(crate) location: String,
    pub(crate) required: bool,
    /// Bare contract datatype token (no `xsd:` prefix).
    pub(crate) datatype: String,
    pub(crate) description: Option<String>,
}

/// One declared response variant (canonical `wf:Response`, legacy `api:Response`).
#[derive(Debug, Clone)]
pub(crate) struct ApiResponse {
    pub(crate) status_code: u16,
    pub(crate) description: String,
}

/// The Operation → Choreograph workflow binding (canonical `wf:WorkflowBinding`,
/// legacy `api:WorkflowBinding`). `input_schema` / `output_schema` are JSON-Schema
/// STRING LITERALS (verbatim JSON content) — the CA-1 boundary. They are parsed
/// and embedded UNCHANGED into `components/schemas`.
#[derive(Debug, Clone)]
pub(crate) struct ApiBinding {
    pub(crate) workflow_name: String,
    /// Canonical durable wf:Workflow subject (legacy bindings may omit it).
    pub(crate) workflow_uri: Option<String>,
    /// Pinned executable-definition digest (legacy bindings may omit it).
    pub(crate) definition_digest: Option<String>,
    /// `gated` | `dumb` (CA-1 §3). Surfaced as the `x-executor` extension.
    pub(crate) executor: String,
    pub(crate) controller: Option<String>,
    pub(crate) model_use: Option<String>,
    pub(crate) isolation: Option<String>,
    pub(crate) effects: Option<String>,
    pub(crate) reproducibility: Option<String>,
    pub(crate) runtime_kind: Option<String>,
    pub(crate) capabilities: Vec<String>,
    pub(crate) minimum_role: Option<String>,
    pub(crate) dangerous_ops: Option<bool>,
    pub(crate) may_invoke_children: Option<bool>,
    /// JSON Schema as a verbatim string literal, or `None`.
    pub(crate) input_schema: Option<String>,
    /// JSON Schema as a verbatim string literal, or `None`.
    pub(crate) output_schema: Option<String>,
}

impl ApiBinding {
    /// Canonical orthogonal execution face, with the old binary executor used
    /// only to fill conservative migration defaults.
    pub(crate) fn execution_extension(&self) -> Value {
        let legacy_program = self.executor == "dumb";
        let controller = self.controller.as_deref().unwrap_or(if legacy_program {
            "program"
        } else {
            "composite"
        });
        let model_use = self
            .model_use
            .as_deref()
            .unwrap_or(if controller == "program" {
                "none"
            } else {
                "agentic"
            });
        let isolation = self.isolation.as_deref().unwrap_or("sandbox");
        let effects = self.effects.as_deref().unwrap_or(if legacy_program {
            "pure"
        } else {
            "capability-bound"
        });
        let reproducibility = self
            .reproducibility
            .as_deref()
            .unwrap_or(if legacy_program {
                "pure"
            } else {
                "recorded-external"
            });
        let mut execution = Map::new();
        execution.insert("schema".into(), json!("choreograph.execution-binding.v1"));
        execution.insert("controller".into(), json!(controller));
        execution.insert("modelUse".into(), json!(model_use));
        execution.insert("isolation".into(), json!(isolation));
        execution.insert("effects".into(), json!(effects));
        execution.insert("reproducibility".into(), json!(reproducibility));
        if let Some(runtime_kind) = self
            .runtime_kind
            .as_deref()
            .or((controller == "program" && isolation == "sandbox").then_some("node"))
        {
            execution.insert("runtime".into(), json!({ "kind": runtime_kind }));
        }
        execution.insert("capabilities".into(), json!(self.capabilities));
        execution.insert(
            "authority".into(),
            json!({
                "minimumRole": self.minimum_role.as_deref().unwrap_or("editor"),
                "dangerousOps": self.dangerous_ops.unwrap_or(false),
            }),
        );
        execution.insert(
            "mayInvokeChildren".into(),
            json!(self.may_invoke_children.unwrap_or(controller != "program")),
        );
        Value::Object(execution)
    }
}

/// One published operation (canonical `wf:Operation` or legacy `api:Operation` +
/// its linked parameters/responses/binding). In the LIVE path this is read from
/// `:projection:api` via SPARQL by the caller (Seq 10, deferred); here it is the
/// pure input the face projects.
#[derive(Debug, Clone)]
pub(crate) struct ApiOperation {
    pub(crate) operation_id: String,
    /// HTTP method (any case); lowercased for the OpenAPI PathItem key.
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) deprecated: bool,
    pub(crate) parameters: Vec<ApiParameter>,
    pub(crate) responses: Vec<ApiResponse>,
    pub(crate) binding: Option<ApiBinding>,
}

/// One declared server (canonical `wf:Server`, legacy `api:Server`).
#[derive(Debug, Clone)]
pub(crate) struct ApiServer {
    pub(crate) url: String,
    pub(crate) description: Option<String>,
}

/// The full published API surface — the set of `api:` instances the OpenAPI face
/// projects. In the LIVE path the caller reads these out of `:projection:api`; the
/// materializer stays pure over this value.
#[derive(Debug, Clone, Default)]
pub(crate) struct ApiSurface {
    pub(crate) operations: Vec<ApiOperation>,
    pub(crate) servers: Vec<ApiServer>,
}

/// The OpenAPI version this face emits. 3.0.3 is the widest-supported 3.0.x patch.
const OPENAPI_VERSION: &str = "3.0.3";

/// Map a bare contract datatype token to the OpenAPI primitive `type` (+ optional
/// `format`) for a Parameter schema. This is the ONLY place the contract alphabet
/// touches OpenAPI, and it is confined to PARAMETER schemas — the workflow I/O
/// JSON Schemas pass through verbatim and are NEVER mapped here. Unknown tokens
/// fall back to `string` (the OpenAPI-safe default for an opaque scalar).
fn datatype_to_openapi(datatype: &str) -> Value {
    match datatype {
        "string" | "uri" => json!({ "type": "string" }),
        "integer" => json!({ "type": "integer", "format": "int32" }),
        "long" => json!({ "type": "integer", "format": "int64" }),
        "float" => json!({ "type": "number", "format": "float" }),
        "double" => json!({ "type": "number", "format": "double" }),
        "boolean" => json!({ "type": "boolean" }),
        "dateTime" => json!({ "type": "string", "format": "date-time" }),
        _ => json!({ "type": "string" }),
    }
}

/// The `components/schemas` key for an operation's request body schema.
fn input_schema_name(operation_id: &str) -> String {
    format!("{operation_id}Input")
}

/// The `components/schemas` key for an operation's success response schema.
fn output_schema_name(operation_id: &str) -> String {
    format!("{operation_id}Output")
}

/// Parse a JSON-Schema STRING LITERAL into a [`Value`], embedding it VERBATIM. A
/// malformed literal is a loud halt (the CA-1 boundary: Emporium does not attempt
/// to repair a workflow author's broken schema, it refuses to publish it).
fn parse_schema_literal(operation_id: &str, role: &str, literal: &str) -> Result<Value, String> {
    serde_json::from_str(literal)
        .map_err(|e| format!("operation '{operation_id}' {role} schema is not valid JSON: {e}"))
}

/// Emit the OpenAPI 3.x document (as a [`serde_json::Value`]) for a published API
/// surface. PURE function of `(contract, surface)` — no I/O, no store, deterministic.
///
/// Returns `Err` (a loud halt) when a binding carries a JSON-Schema literal that is
/// not valid JSON: the OpenAPI face refuses to emit a half-formed spec. All
/// well-formed surfaces emit a complete, valid OpenAPI 3.0.x document.
pub(crate) fn vocab_to_openapi(
    contract: &VocabularyContract,
    surface: &ApiSurface,
) -> Result<Value, String> {
    let mut components_schemas = Map::new();
    let mut paths: Map<String, Value> = Map::new();

    for op in &surface.operations {
        // --- parameters -------------------------------------------------------
        let parameters: Vec<Value> = op
            .parameters
            .iter()
            .map(|p| {
                // Path parameters are ALWAYS required in OpenAPI regardless of the
                // declared flag (the spec mandates it; a non-required path param is
                // invalid). Honor that here so the emitted spec validates.
                let required = p.required || p.location == "path";
                let mut entry = Map::new();
                entry.insert("name".into(), json!(p.name));
                entry.insert("in".into(), json!(p.location));
                if let Some(desc) = &p.description {
                    entry.insert("description".into(), json!(desc));
                }
                entry.insert("required".into(), json!(required));
                entry.insert("schema".into(), datatype_to_openapi(&p.datatype));
                Value::Object(entry)
            })
            .collect();

        // --- requestBody (from the binding's inputSchema, VERBATIM) ----------
        let mut request_body: Option<Value> = None;
        if let Some(binding) = &op.binding {
            if let Some(literal) = &binding.input_schema {
                let parsed = parse_schema_literal(&op.operation_id, "input", literal)?;
                let schema_name = input_schema_name(&op.operation_id);
                // Embed the parsed JSON Schema UNCHANGED — zero translation.
                components_schemas.insert(schema_name.clone(), parsed);
                request_body = Some(json!({
                    "required": true,
                    "content": {
                        "application/json": {
                            "schema": { "$ref": format!("#/components/schemas/{schema_name}") }
                        }
                    }
                }));
            }
        }

        // --- responses --------------------------------------------------------
        let mut responses = Map::new();

        // The success response carries the binding's outputSchema (VERBATIM) when
        // present. If the surface declares an explicit 200 api:Response we use its
        // description; otherwise a sensible default.
        let success_desc = op
            .responses
            .iter()
            .find(|r| r.status_code == 200)
            .map(|r| r.description.clone())
            .unwrap_or_else(|| "Successful response".to_string());

        let mut success = Map::new();
        success.insert("description".into(), json!(success_desc));
        if let Some(binding) = &op.binding {
            if let Some(literal) = &binding.output_schema {
                let parsed = parse_schema_literal(&op.operation_id, "output", literal)?;
                let schema_name = output_schema_name(&op.operation_id);
                components_schemas.insert(schema_name.clone(), parsed);
                success.insert(
                    "content".into(),
                    json!({
                        "application/json": {
                            "schema": { "$ref": format!("#/components/schemas/{schema_name}") }
                        }
                    }),
                );
            }
        }
        responses.insert("200".into(), Value::Object(success));

        // Any additional declared (non-200) responses — typically error variants.
        for resp in &op.responses {
            if resp.status_code == 200 {
                continue;
            }
            responses.insert(
                resp.status_code.to_string(),
                json!({ "description": resp.description }),
            );
        }

        // --- assemble the operation object -----------------------------------
        let mut operation = Map::new();
        operation.insert("operationId".into(), json!(op.operation_id));
        if let Some(summary) = &op.summary {
            operation.insert("summary".into(), json!(summary));
        }
        if let Some(desc) = &op.description {
            operation.insert("description".into(), json!(desc));
        }
        if op.deprecated {
            operation.insert("deprecated".into(), json!(true));
        }
        // Discovery hooks for Choreograph + API-discovery tools (the executor mode
        // and the workflow this operation binds to).
        if let Some(binding) = &op.binding {
            operation.insert("x-workflow-name".into(), json!(binding.workflow_name));
            operation.insert("x-executor".into(), json!(binding.executor));
            operation.insert("x-execution".into(), binding.execution_extension());
            if let Some(workflow_uri) = &binding.workflow_uri {
                operation.insert("x-workflow-uri".into(), json!(workflow_uri));
            }
            if let Some(definition_digest) = &binding.definition_digest {
                operation.insert("x-definition-digest".into(), json!(definition_digest));
            }
        }
        if !parameters.is_empty() {
            operation.insert("parameters".into(), Value::Array(parameters));
        }
        if let Some(rb) = request_body {
            operation.insert("requestBody".into(), rb);
        }
        operation.insert("responses".into(), Value::Object(responses));

        // --- merge into the PathItem (a path may carry several methods) -------
        let method_key = op.method.to_ascii_lowercase();
        let path_item = paths
            .entry(op.path.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(item) = path_item {
            item.insert(method_key, Value::Object(operation));
        }
    }

    // --- info block (from the contract metadata) -----------------------------
    let info = json!({
        "title": contract.title,
        "version": contract.version,
        "description": contract.description,
    });

    // --- servers (from api:Server instances) ---------------------------------
    let servers: Vec<Value> = surface
        .servers
        .iter()
        .map(|s| {
            let mut entry = Map::new();
            entry.insert("url".into(), json!(s.url));
            if let Some(desc) = &s.description {
                entry.insert("description".into(), json!(desc));
            }
            Value::Object(entry)
        })
        .collect();

    // --- assemble the root document ------------------------------------------
    let mut root = Map::new();
    root.insert("openapi".into(), json!(OPENAPI_VERSION));
    root.insert("info".into(), info);
    if !servers.is_empty() {
        root.insert("servers".into(), Value::Array(servers));
    }
    root.insert("paths".into(), Value::Object(paths));
    root.insert(
        "components".into(),
        json!({ "schemas": Value::Object(components_schemas) }),
    );

    Ok(Value::Object(root))
}

// ===========================================================================
// The READ path — reconstruct an ApiSurface from the published `:projection:api`
// instances (EA-3 Seq 9 / UC-2, the POPULATED serving half).
//
// `vocab_to_openapi` is the PURE materializer over `(contract, surface)`. This is
// its companion: it reads the born-RDF the generic spine published into
// `:projection:api` (`api:Operation` + linked `api:Parameter` / `api:Response` /
// `api:WorkflowBinding` + global `api:Server` instances) and reconstructs the
// `ApiSurface` value the materializer projects. The two compose into the LIVE
// serving path: `read_api_surface(store, graph) → vocab_to_openapi(contract, &surface)`
// → the POPULATED OpenAPI document (not the metadata shell `render_body` emits
// without a graph).
//
// This is a DETERMINISTIC SPARQL read — no workflow, no LLM, no invocation. The
// LIVE endpoint (`handle_api_operation` EXECUTING the bound workflow via
// Choreograph) is CA-1's, gated on CA-1's WF-4 — explicitly OUT of scope and
// honestly deferred (a workflow is never run here).
//
// `api:inputSchema` / `api:outputSchema` come back as VERBATIM JSON-Schema string
// literals: this reader carries them UNCHANGED into the `ApiBinding`; the
// materializer parses + embeds them. A malformed schema is the materializer's loud
// halt, never silently repaired here (the CA-1 boundary).
// ===========================================================================

/// The `api:` namespace (matches `sophia-api.golden.json` → `namespaces.api`).
const API_NS: &str = "http://sophia.ai/api#";
/// The canonical Workflow namespace. Workflow owns invocation/API terms; `api:`
/// remains readable as the compatibility publication face.
const WF_NS: &str = "http://mnemosyne.dev/workflow#";

/// The reserved, materializer-only `:projection:api` named-graph IRI for a graph —
/// `urn:mnemosyne:local:graph:{id}:projection:api`. Parallel to
/// `memory_projection_graph_iri` / `chamber_projection_graph_iri`; reserved via the
/// `:projection:` prefix (the user:rdf SPARQL service refuses it, so the sink is
/// materializer-owned — the generic spine writes it direct-on-store).
pub(crate) fn api_projection_graph_iri(graph_id: &str) -> String {
    format!("{}:projection:api", graph_subject(graph_id))
}

/// Open the per-graph oxigraph Store for `graph_id` (resolve the graph dir first —
/// `open_graph_store` takes a `&Path`). The same process-cached store handle the
/// generic spine writes `:projection:api` into; the OpenAPI serving route reads it.
pub(crate) fn open_api_store(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
) -> Result<std::sync::Arc<Store>, String> {
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id)
        .map_err(|e| format!("resolve graph dir: {e}"))?;
    crate::rdf_service::open_graph_store(&graph_dir)
}

/// Extract the lexical value of a literal object term (strip the `"…"^^<dt>` /
/// `"…"@lang` decorations). A non-literal is a loud error — a structural fault in
/// the published triples, not a thing this reader papers over.
fn literal_value(term: &Term) -> Result<String, String> {
    match term {
        Term::Literal(l) => Ok(l.value().to_string()),
        other => Err(format!("expected a literal, got {other}")),
    }
}

/// Parse a boolean literal's lexical value (`api:deprecated` / `api:required`).
fn bool_value(term: &Term) -> Result<bool, String> {
    literal_value(term).and_then(|v| {
        v.parse::<bool>()
            .map_err(|e| format!("parse boolean '{v}': {e}"))
    })
}

/// Parse an integer literal's lexical value (`api:statusCode`).
fn u16_value(term: &Term) -> Result<u16, String> {
    literal_value(term).and_then(|v| {
        v.parse::<u16>()
            .map_err(|e| format!("parse integer '{v}': {e}"))
    })
}

/// Extract a URI object's IRI string. `api:url` is declared `datatype: uri`, so the
/// generic spine renders it as a NamedNode (not a string literal) — the OpenAPI
/// `servers[*].url` is a plain string, so we lift the IRI back to a `String`.
fn uri_value(term: &Term) -> Result<String, String> {
    match term {
        Term::NamedNode(n) => Ok(n.as_str().to_string()),
        other => Err(format!("expected a NamedNode URI, got {other}")),
    }
}

fn merge_binding_string(
    current: &mut Option<String>,
    incoming: Option<String>,
    field: &str,
    operation_id: &str,
) -> Result<(), String> {
    let Some(incoming) = incoming else {
        return Ok(());
    };
    match current {
        Some(existing) if existing != &incoming => Err(format!(
            "contested executable binding for operation '{operation_id}': conflicting {field} values"
        )),
        Some(_) => Ok(()),
        None => {
            *current = Some(incoming);
            Ok(())
        }
    }
}

fn merge_binding_bool(
    current: &mut Option<bool>,
    incoming: Option<bool>,
    field: &str,
    operation_id: &str,
) -> Result<(), String> {
    let Some(incoming) = incoming else {
        return Ok(());
    };
    match current {
        Some(existing) if *existing != incoming => Err(format!(
            "contested executable binding for operation '{operation_id}': conflicting {field} values"
        )),
        Some(_) => Ok(()),
        None => {
            *current = Some(incoming);
            Ok(())
        }
    }
}

/// Read the published API surface from a graph's `:projection:api` sink and
/// reconstruct the [`ApiSurface`] the OpenAPI face projects. Pure-deterministic
/// over the store: one unified SPARQL SELECT collects every `api:Operation` and its
/// linked `api:Parameter` / `api:Response` / `api:WorkflowBinding`, plus the global
/// `api:Server` instances; the Cartesian product is then deduplicated into a stable
/// surface (operations ordered by `operationId`; parameters/responses/servers
/// deduplicated by their identity key).
///
/// An empty `:projection:api` graph (no published operations) is NOT an error — it
/// returns `ApiSurface { operations: [], servers: [] }`, which the materializer
/// renders as a valid empty-paths OpenAPI document. `Err` is a loud halt on a
/// genuine fault: a SPARQL parse/execute error or a structurally-broken triple (a
/// required field carrying a non-literal, an unparseable status code, …).
pub(crate) fn read_api_surface(store: &Store, graph_id: &str) -> Result<ApiSurface, String> {
    let api_graph = api_projection_graph_iri(graph_id);

    // One unified SELECT: every canonical wf:Operation or compatibility
    // api:Operation, LEFT-joined to its Parameters / Responses / Binding, and the
    // global Servers (no per-operation link). OPTIONAL → unbound vars for absent
    // facets; the reconstruction below dedups the Cartesian unwind.
    let query = format!(
        "PREFIX api: <{API_NS}>\n\
         PREFIX wf: <{WF_NS}>\n\
         PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>\n\
         SELECT\n\
           ?op ?operationId ?method ?path ?summary ?description ?deprecated\n\
           ?param ?paramName ?paramIn ?paramRequired ?paramDatatype ?paramDesc\n\
           ?resp ?respStatus ?respDesc\n\
           ?binding ?workflowName ?workflowUri ?definitionDigest ?executor\n\
           ?controller ?modelUse ?isolation ?effects ?reproducibility ?runtimeKind\n\
           ?capability ?minimumRole ?dangerousOps ?mayInvokeChildren ?inputSchema ?outputSchema\n\
           ?server ?serverUrl ?serverDesc\n\
         WHERE {{ GRAPH <{api_graph}> {{\n\
           ?op a ?operationType .\n\
           VALUES ?operationType {{ wf:Operation api:Operation }}\n\
           OPTIONAL {{ ?op wf:operationId ?wfOperationId . }}\n\
           OPTIONAL {{ ?op api:operationId ?apiOperationId . }}\n\
           BIND(COALESCE(?wfOperationId, ?apiOperationId) AS ?operationId)\n\
           OPTIONAL {{ ?op wf:method ?wfMethod . }}\n\
           OPTIONAL {{ ?op api:method ?apiMethod . }}\n\
           BIND(COALESCE(?wfMethod, ?apiMethod) AS ?method)\n\
           OPTIONAL {{ ?op wf:path ?wfPath . }}\n\
           OPTIONAL {{ ?op api:path ?apiPath . }}\n\
           BIND(COALESCE(?wfPath, ?apiPath) AS ?path)\n\
           OPTIONAL {{ ?op wf:summary ?wfSummary . }}\n\
           OPTIONAL {{ ?op api:summary ?apiSummary . }}\n\
           BIND(COALESCE(?wfSummary, ?apiSummary) AS ?summary)\n\
           OPTIONAL {{ ?op wf:description ?wfDescription . }}\n\
           OPTIONAL {{ ?op api:description ?apiDescription . }}\n\
           BIND(COALESCE(?wfDescription, ?apiDescription) AS ?description)\n\
           OPTIONAL {{ ?op wf:deprecated ?wfDeprecated . }}\n\
           OPTIONAL {{ ?op api:deprecated ?apiDeprecated . }}\n\
           BIND(COALESCE(?wfDeprecated, ?apiDeprecated) AS ?deprecated)\n\
           FILTER(BOUND(?operationId) && BOUND(?method) && BOUND(?path))\n\
           OPTIONAL {{\n\
             ?param a ?paramType .\n\
             VALUES ?paramType {{ wf:Parameter api:Parameter }}\n\
             {{ ?param wf:ofOperation ?op . }} UNION {{ ?param api:ofOperation ?op . }}\n\
             OPTIONAL {{ ?param wf:name ?wfParamName . }}\n\
             OPTIONAL {{ ?param api:name ?apiParamName . }}\n\
             BIND(COALESCE(?wfParamName, ?apiParamName) AS ?paramName)\n\
             OPTIONAL {{ ?param wf:in ?wfParamIn . }}\n\
             OPTIONAL {{ ?param api:in ?apiParamIn . }}\n\
             BIND(COALESCE(?wfParamIn, ?apiParamIn) AS ?paramIn)\n\
             OPTIONAL {{ ?param wf:required ?wfParamRequired . }}\n\
             OPTIONAL {{ ?param api:required ?apiParamRequired . }}\n\
             BIND(COALESCE(?wfParamRequired, ?apiParamRequired) AS ?paramRequired)\n\
             OPTIONAL {{ ?param wf:datatype ?wfParamDatatype . }}\n\
             OPTIONAL {{ ?param api:datatype ?apiParamDatatype . }}\n\
             BIND(COALESCE(?wfParamDatatype, ?apiParamDatatype) AS ?paramDatatype)\n\
             OPTIONAL {{ ?param wf:description ?wfParamDesc . }}\n\
             OPTIONAL {{ ?param api:description ?apiParamDesc . }}\n\
             BIND(COALESCE(?wfParamDesc, ?apiParamDesc) AS ?paramDesc)\n\
             FILTER(BOUND(?paramName) && BOUND(?paramIn))\n\
           }}\n\
           OPTIONAL {{\n\
             ?resp a ?respType .\n\
             VALUES ?respType {{ wf:Response api:Response }}\n\
             {{ ?resp wf:ofOperation ?op . }} UNION {{ ?resp api:ofOperation ?op . }}\n\
             OPTIONAL {{ ?resp wf:statusCode ?wfRespStatus . }}\n\
             OPTIONAL {{ ?resp api:statusCode ?apiRespStatus . }}\n\
             BIND(COALESCE(?wfRespStatus, ?apiRespStatus) AS ?respStatus)\n\
             OPTIONAL {{ ?resp wf:description ?wfRespDesc . }}\n\
             OPTIONAL {{ ?resp api:description ?apiRespDesc . }}\n\
             BIND(COALESCE(?wfRespDesc, ?apiRespDesc) AS ?respDesc)\n\
             FILTER(BOUND(?respStatus) && BOUND(?respDesc))\n\
           }}\n\
           OPTIONAL {{\n\
             ?binding a ?bindingType .\n\
             VALUES ?bindingType {{ wf:WorkflowBinding api:WorkflowBinding }}\n\
             {{ ?binding wf:bindsOperation ?op . }} UNION {{ ?binding api:bindsOperation ?op . }}\n\
             OPTIONAL {{ ?binding wf:workflowName ?wfWorkflowName . }}\n\
             OPTIONAL {{ ?binding api:workflowName ?apiWorkflowName . }}\n\
             BIND(COALESCE(?wfWorkflowName, ?apiWorkflowName) AS ?workflowName)\n\
             OPTIONAL {{ ?binding wf:bindsWorkflow ?workflowUri . }}\n\
             OPTIONAL {{ ?binding wf:definitionDigest ?definitionDigest . }}\n\
             OPTIONAL {{ ?binding wf:executor ?wfExecutor . }}\n\
             OPTIONAL {{ ?binding api:executor ?apiExecutor . }}\n\
             BIND(COALESCE(?wfExecutor, ?apiExecutor) AS ?executor)\n\
             OPTIONAL {{ ?binding wf:controller ?controller . }}\n\
             OPTIONAL {{ ?binding wf:modelUse ?modelUse . }}\n\
             OPTIONAL {{ ?binding wf:isolation ?isolation . }}\n\
             OPTIONAL {{ ?binding wf:effects ?effects . }}\n\
             OPTIONAL {{ ?binding wf:reproducibility ?reproducibility . }}\n\
             OPTIONAL {{ ?binding wf:runtimeKind ?runtimeKind . }}\n\
             OPTIONAL {{ ?binding wf:capability ?capability . }}\n\
             OPTIONAL {{ ?binding wf:minimumRole ?minimumRole . }}\n\
             OPTIONAL {{ ?binding wf:dangerousOps ?dangerousOps . }}\n\
             OPTIONAL {{ ?binding wf:mayInvokeChildren ?mayInvokeChildren . }}\n\
             OPTIONAL {{ ?binding wf:inputSchema ?wfInputSchema . }}\n\
             OPTIONAL {{ ?binding api:inputSchema ?apiInputSchema . }}\n\
             BIND(COALESCE(?wfInputSchema, ?apiInputSchema) AS ?inputSchema)\n\
             OPTIONAL {{ ?binding wf:outputSchema ?wfOutputSchema . }}\n\
             OPTIONAL {{ ?binding api:outputSchema ?apiOutputSchema . }}\n\
             BIND(COALESCE(?wfOutputSchema, ?apiOutputSchema) AS ?outputSchema)\n\
             FILTER(BOUND(?workflowName) && BOUND(?executor))\n\
           }}\n\
           OPTIONAL {{\n\
             ?server a ?serverType .\n\
             VALUES ?serverType {{ wf:Server api:Server }}\n\
             OPTIONAL {{ ?server wf:url ?wfServerUrl . }}\n\
             OPTIONAL {{ ?server api:url ?apiServerUrl . }}\n\
             BIND(COALESCE(?wfServerUrl, ?apiServerUrl) AS ?serverUrl)\n\
             OPTIONAL {{ ?server wf:description ?wfServerDesc . }}\n\
             OPTIONAL {{ ?server api:description ?apiServerDesc . }}\n\
             BIND(COALESCE(?wfServerDesc, ?apiServerDesc) AS ?serverDesc)\n\
             FILTER(BOUND(?serverUrl))\n\
           }}\n\
         }} }}"
    );

    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| format!("parse api surface query: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute api surface query: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("api surface query expected SELECT solutions".to_string()),
    };

    // Operations keyed by operationId — BTreeMap gives a stable (alphabetical),
    // deterministic surface order independent of SPARQL solution order.
    let mut ops: BTreeMap<String, ApiOperation> = BTreeMap::new();
    // Executable selection is fail-closed: two subjects claiming one logical
    // operation id, or two bindings claiming one operation, are a contested
    // object face. Publication/invocation must not pick a SPARQL row by accident.
    let mut operation_subjects: BTreeMap<String, String> = BTreeMap::new();
    let mut binding_subjects: BTreeMap<String, String> = BTreeMap::new();
    // Servers are GLOBAL (no per-operation link) — they repeat in every row; dedup
    // by URL. BTreeMap keeps server order deterministic too.
    let mut servers: BTreeMap<String, ApiServer> = BTreeMap::new();

    for sol in solutions {
        let sol = sol.map_err(|e| format!("api surface row: {e}"))?;

        // --- the Operation this row carries (always bound) -------------------
        let operation_id = literal_value(
            sol.get("operationId")
                .ok_or("api surface row missing ?operationId")?,
        )?;
        let operation_subject = uri_value(sol.get("op").ok_or("api surface row missing ?op")?)?;
        if let Some(existing) = operation_subjects.get(&operation_id) {
            if existing != &operation_subject {
                return Err(format!(
                    "contested operation face for '{operation_id}': subjects '{existing}' and '{operation_subject}'"
                ));
            }
        } else {
            operation_subjects.insert(operation_id.clone(), operation_subject);
        }
        let entry = match ops.get_mut(&operation_id) {
            Some(existing) => existing,
            None => {
                let method = literal_value(
                    sol.get("method")
                        .ok_or("api:Operation row missing ?method")?,
                )?;
                let path =
                    literal_value(sol.get("path").ok_or("api:Operation row missing ?path")?)?;
                let summary = sol.get("summary").map(literal_value).transpose()?;
                let description = sol.get("description").map(literal_value).transpose()?;
                let deprecated = sol
                    .get("deprecated")
                    .map(bool_value)
                    .transpose()?
                    .unwrap_or(false);
                ops.insert(
                    operation_id.clone(),
                    ApiOperation {
                        operation_id: operation_id.clone(),
                        method,
                        path,
                        summary,
                        description,
                        deprecated,
                        parameters: Vec::new(),
                        responses: Vec::new(),
                        binding: None,
                    },
                );
                ops.get_mut(&operation_id).expect("just inserted")
            }
        };

        // --- a Parameter on this Operation (dedup by name) -------------------
        if let Some(name_term) = sol.get("paramName") {
            let name = literal_value(name_term)?;
            if !entry.parameters.iter().any(|p| p.name == name) {
                let location = literal_value(
                    sol.get("paramIn")
                        .ok_or("api:Parameter row missing ?paramIn")?,
                )?;
                let required = sol
                    .get("paramRequired")
                    .map(bool_value)
                    .transpose()?
                    .unwrap_or(false);
                let datatype = sol
                    .get("paramDatatype")
                    .map(literal_value)
                    .transpose()?
                    .unwrap_or_else(|| "string".to_string());
                let description = sol.get("paramDesc").map(literal_value).transpose()?;
                entry.parameters.push(ApiParameter {
                    name,
                    location,
                    required,
                    datatype,
                    description,
                });
            }
        }

        // --- a Response on this Operation (dedup by status code) -------------
        if let Some(status_term) = sol.get("respStatus") {
            let status_code = u16_value(status_term)?;
            if !entry.responses.iter().any(|r| r.status_code == status_code) {
                let description = literal_value(
                    sol.get("respDesc")
                        .ok_or("api:Response row missing ?respDesc")?,
                )?;
                entry.responses.push(ApiResponse {
                    status_code,
                    description,
                });
            }
        }

        // --- the WorkflowBinding on this Operation (exactly zero or one) -----
        if let Some(binding_term) = sol.get("binding") {
            let binding_subject = uri_value(binding_term)?;
            if let Some(existing) = binding_subjects.get(&operation_id) {
                if existing != &binding_subject {
                    return Err(format!(
                        "contested executable binding for operation '{operation_id}': subjects '{existing}' and '{binding_subject}'"
                    ));
                }
            } else {
                binding_subjects.insert(operation_id.clone(), binding_subject);
            }

            let workflow_name = literal_value(
                sol.get("workflowName")
                    .ok_or("api:WorkflowBinding row missing ?workflowName")?,
            )?;
            let executor = literal_value(
                sol.get("executor")
                    .ok_or("api:WorkflowBinding row missing ?executor")?,
            )?;
            let workflow_uri = sol.get("workflowUri").map(uri_value).transpose()?;
            let definition_digest = sol.get("definitionDigest").map(literal_value).transpose()?;
            let controller = sol.get("controller").map(literal_value).transpose()?;
            let model_use = sol.get("modelUse").map(literal_value).transpose()?;
            let isolation = sol.get("isolation").map(literal_value).transpose()?;
            let effects = sol.get("effects").map(literal_value).transpose()?;
            let reproducibility = sol.get("reproducibility").map(literal_value).transpose()?;
            let runtime_kind = sol.get("runtimeKind").map(literal_value).transpose()?;
            let capability = sol.get("capability").map(literal_value).transpose()?;
            let minimum_role = sol.get("minimumRole").map(literal_value).transpose()?;
            let dangerous_ops = sol.get("dangerousOps").map(bool_value).transpose()?;
            let may_invoke_children = sol.get("mayInvokeChildren").map(bool_value).transpose()?;
            let input_schema = sol.get("inputSchema").map(literal_value).transpose()?;
            let output_schema = sol.get("outputSchema").map(literal_value).transpose()?;

            if let Some(binding) = entry.binding.as_mut() {
                if binding.workflow_name != workflow_name || binding.executor != executor {
                    return Err(format!(
                        "contested executable binding for operation '{operation_id}': conflicting workflowName/executor values"
                    ));
                }
                merge_binding_string(
                    &mut binding.workflow_uri,
                    workflow_uri,
                    "bindsWorkflow",
                    &operation_id,
                )?;
                merge_binding_string(
                    &mut binding.definition_digest,
                    definition_digest,
                    "definitionDigest",
                    &operation_id,
                )?;
                merge_binding_string(
                    &mut binding.controller,
                    controller,
                    "controller",
                    &operation_id,
                )?;
                merge_binding_string(&mut binding.model_use, model_use, "modelUse", &operation_id)?;
                merge_binding_string(
                    &mut binding.isolation,
                    isolation,
                    "isolation",
                    &operation_id,
                )?;
                merge_binding_string(&mut binding.effects, effects, "effects", &operation_id)?;
                merge_binding_string(
                    &mut binding.reproducibility,
                    reproducibility,
                    "reproducibility",
                    &operation_id,
                )?;
                merge_binding_string(
                    &mut binding.runtime_kind,
                    runtime_kind,
                    "runtimeKind",
                    &operation_id,
                )?;
                merge_binding_string(
                    &mut binding.minimum_role,
                    minimum_role,
                    "minimumRole",
                    &operation_id,
                )?;
                merge_binding_bool(
                    &mut binding.dangerous_ops,
                    dangerous_ops,
                    "dangerousOps",
                    &operation_id,
                )?;
                merge_binding_bool(
                    &mut binding.may_invoke_children,
                    may_invoke_children,
                    "mayInvokeChildren",
                    &operation_id,
                )?;
                merge_binding_string(
                    &mut binding.input_schema,
                    input_schema,
                    "inputSchema",
                    &operation_id,
                )?;
                merge_binding_string(
                    &mut binding.output_schema,
                    output_schema,
                    "outputSchema",
                    &operation_id,
                )?;
                if let Some(capability) = capability {
                    if !binding.capabilities.contains(&capability) {
                        binding.capabilities.push(capability);
                        binding.capabilities.sort();
                    }
                }
            } else {
                entry.binding = Some(ApiBinding {
                    workflow_name,
                    workflow_uri,
                    definition_digest,
                    executor,
                    controller,
                    model_use,
                    isolation,
                    effects,
                    reproducibility,
                    runtime_kind,
                    capabilities: capability.into_iter().collect(),
                    minimum_role,
                    dangerous_ops,
                    may_invoke_children,
                    input_schema,
                    output_schema,
                });
            }
        }

        // --- a global Server (dedup by url) ----------------------------------
        if let Some(url_term) = sol.get("serverUrl") {
            let url = uri_value(url_term)?;
            servers.entry(url.clone()).or_insert_with(|| {
                let description = sol
                    .get("serverDesc")
                    .map(literal_value)
                    .transpose()
                    .ok()
                    .flatten();
                ApiServer { url, description }
            });
        }
    }

    Ok(ApiSurface {
        operations: ops.into_values().collect(),
        servers: servers.into_values().collect(),
    })
}

/// Resolve one logical Operation from the graph-authored API surface. Because
/// `read_api_surface` detects duplicate operation/binding subjects, success here
/// is also the executable-object stance: exactly one uncontested binding face.
pub(crate) fn read_api_operation(
    store: &Store,
    graph_id: &str,
    operation_id: &str,
) -> Result<ApiOperation, String> {
    let mut matches = read_api_surface(store, graph_id)?
        .operations
        .into_iter()
        .filter(|operation| operation.operation_id == operation_id);
    let operation = matches
        .next()
        .ok_or_else(|| format!("operation_not_found: '{operation_id}'"))?;
    if matches.next().is_some() {
        return Err(format!(
            "contested operation face for '{operation_id}': more than one resolved operation"
        ));
    }
    Ok(operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::contract::get_vocabulary;
    use oxigraph::model::{GraphName, Literal, NamedNode, Quad, Term as OxTerm};

    /// A small, real published surface authored against the `sophia-api` vocab's
    /// namespace: two operations (one POST with full I/O JSON-Schema literals, one
    /// GET with a path param + dumb executor), and one server. The JSON-Schema
    /// literals are the load-bearing fixture — they must survive VERBATIM.
    fn example_surface() -> ApiSurface {
        let create_input = r#"{"type":"object","required":["title"],"properties":{"title":{"type":"string"},"due":{"type":"string","format":"date-time"}}}"#;
        let create_output =
            r#"{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"}}}"#;
        let get_output = r#"{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"done":{"type":"boolean"}}}"#;

        ApiSurface {
            operations: vec![
                ApiOperation {
                    operation_id: "createTask".to_string(),
                    method: "POST".to_string(),
                    path: "/tasks".to_string(),
                    summary: Some("Create a task".to_string()),
                    description: Some("Files a new task into the graph.".to_string()),
                    deprecated: false,
                    parameters: vec![],
                    responses: vec![
                        ApiResponse {
                            status_code: 200,
                            description: "The created task".to_string(),
                        },
                        ApiResponse {
                            status_code: 400,
                            description: "Invalid input (invalid_input)".to_string(),
                        },
                    ],
                    binding: Some(ApiBinding {
                        workflow_name: "file-task".to_string(),
                        workflow_uri: Some("urn:sophia:wf:file-task".to_string()),
                        definition_digest: Some("sha256-file-task".to_string()),
                        executor: "gated".to_string(),
                        controller: Some("composite".to_string()),
                        model_use: Some("agentic".to_string()),
                        isolation: Some("sandbox".to_string()),
                        effects: Some("capability-bound".to_string()),
                        reproducibility: Some("recorded-external".to_string()),
                        runtime_kind: None,
                        capabilities: vec!["garden.graph".to_string()],
                        minimum_role: Some("editor".to_string()),
                        dangerous_ops: Some(false),
                        may_invoke_children: Some(true),
                        input_schema: Some(create_input.to_string()),
                        output_schema: Some(create_output.to_string()),
                    }),
                },
                ApiOperation {
                    operation_id: "getTask".to_string(),
                    method: "GET".to_string(),
                    path: "/tasks/{id}".to_string(),
                    summary: None,
                    description: None,
                    deprecated: false,
                    parameters: vec![ApiParameter {
                        name: "id".to_string(),
                        location: "path".to_string(),
                        // Declared non-required, but path params are forced required.
                        required: false,
                        datatype: "string".to_string(),
                        description: Some("the task id".to_string()),
                    }],
                    responses: vec![],
                    binding: Some(ApiBinding {
                        workflow_name: "read-task".to_string(),
                        workflow_uri: None,
                        definition_digest: None,
                        executor: "dumb".to_string(),
                        controller: None,
                        model_use: None,
                        isolation: None,
                        effects: None,
                        reproducibility: None,
                        runtime_kind: None,
                        capabilities: vec![],
                        minimum_role: None,
                        dangerous_ops: None,
                        may_invoke_children: None,
                        input_schema: None,
                        output_schema: Some(get_output.to_string()),
                    }),
                },
            ],
            servers: vec![ApiServer {
                url: "https://api.sophia-labs.com".to_string(),
                description: Some("Production".to_string()),
            }],
        }
    }

    fn sophia_api() -> &'static VocabularyContract {
        get_vocabulary("sophia-api").expect("sophia-api is a registered vocab")
    }

    fn workflow_vocab() -> &'static VocabularyContract {
        get_vocabulary("workflow").expect("workflow is a registered vocab")
    }

    fn insert_named(store: &Store, graph: &str, s: &str, p: &str, o: OxTerm) {
        store
            .insert(&Quad::new(
                NamedNode::new(s).unwrap(),
                NamedNode::new(p).unwrap(),
                o,
                GraphName::NamedNode(NamedNode::new(graph).unwrap()),
            ))
            .unwrap();
    }

    fn iri(value: &str) -> OxTerm {
        OxTerm::NamedNode(NamedNode::new(value).unwrap())
    }

    fn lit(value: &str) -> OxTerm {
        OxTerm::Literal(Literal::new_simple_literal(value))
    }

    fn lit_typed(value: &str, datatype: &str) -> OxTerm {
        OxTerm::Literal(Literal::new_typed_literal(
            value,
            NamedNode::new(datatype).unwrap(),
        ))
    }

    #[test]
    fn emits_valid_openapi_3x_root() {
        let spec = vocab_to_openapi(sophia_api(), &example_surface()).expect("valid surface");
        assert_eq!(spec["openapi"], OPENAPI_VERSION);
        assert!(spec["openapi"].as_str().unwrap().starts_with("3."));
        // info is sourced from the contract metadata.
        assert_eq!(spec["info"]["title"], "Sophia API Publication Vocabulary");
        assert_eq!(spec["info"]["version"], "1.0.0");
        assert!(spec["info"]["description"].is_string());
        assert!(spec["paths"].is_object());
        assert!(spec["components"]["schemas"].is_object());
    }

    #[test]
    fn projects_each_operation_to_a_path_item() {
        let spec = vocab_to_openapi(sophia_api(), &example_surface()).unwrap();
        // POST /tasks
        let create = &spec["paths"]["/tasks"]["post"];
        assert_eq!(create["operationId"], "createTask");
        assert_eq!(create["summary"], "Create a task");
        // GET /tasks/{id}
        let get = &spec["paths"]["/tasks/{id}"]["get"];
        assert_eq!(get["operationId"], "getTask");
    }

    #[test]
    fn carries_x_workflow_name_and_x_executor_discovery_hooks() {
        let spec = vocab_to_openapi(sophia_api(), &example_surface()).unwrap();
        let create = &spec["paths"]["/tasks"]["post"];
        assert_eq!(create["x-workflow-name"], "file-task");
        assert_eq!(create["x-executor"], "gated");
        let get = &spec["paths"]["/tasks/{id}"]["get"];
        assert_eq!(get["x-workflow-name"], "read-task");
        assert_eq!(get["x-executor"], "dumb");
        assert_eq!(create["x-workflow-uri"], "urn:sophia:wf:file-task");
        assert_eq!(create["x-definition-digest"], "sha256-file-task");
        assert_eq!(create["x-execution"]["controller"], "composite");
        assert_eq!(create["x-execution"]["modelUse"], "agentic");
        assert_eq!(create["x-execution"]["capabilities"][0], "garden.graph");
        // Legacy `dumb` is retained as a compatibility face, while the canonical
        // extension expands it into orthogonal conservative defaults.
        assert_eq!(get["x-execution"]["controller"], "program");
        assert_eq!(get["x-execution"]["modelUse"], "none");
        assert_eq!(get["x-execution"]["isolation"], "sandbox");
    }

    #[test]
    fn input_schema_passes_through_verbatim_into_components() {
        let surface = example_surface();
        let literal = surface.operations[0]
            .binding
            .as_ref()
            .unwrap()
            .input_schema
            .clone()
            .unwrap();
        // The expected value is the literal parsed independently — the round-trip
        // check: what we embed must equal the author's JSON Schema exactly.
        let expected: Value = serde_json::from_str(&literal).unwrap();

        let spec = vocab_to_openapi(sophia_api(), &surface).unwrap();
        let embedded = &spec["components"]["schemas"]["createTaskInput"];
        assert_eq!(
            embedded, &expected,
            "input schema must be embedded VERBATIM"
        );

        // And the request body $refs it.
        assert_eq!(
            spec["paths"]["/tasks"]["post"]["requestBody"]["content"]["application/json"]["schema"]
                ["$ref"],
            "#/components/schemas/createTaskInput"
        );
    }

    #[test]
    fn output_schema_passes_through_verbatim_into_the_200_response() {
        let surface = example_surface();
        let literal = surface.operations[1]
            .binding
            .as_ref()
            .unwrap()
            .output_schema
            .clone()
            .unwrap();
        let expected: Value = serde_json::from_str(&literal).unwrap();

        let spec = vocab_to_openapi(sophia_api(), &surface).unwrap();
        let embedded = &spec["components"]["schemas"]["getTaskOutput"];
        assert_eq!(
            embedded, &expected,
            "output schema must be embedded VERBATIM"
        );
        assert_eq!(
            spec["paths"]["/tasks/{id}"]["get"]["responses"]["200"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/getTaskOutput"
        );
    }

    #[test]
    fn path_parameters_are_forced_required() {
        let spec = vocab_to_openapi(sophia_api(), &example_surface()).unwrap();
        let params = spec["paths"]["/tasks/{id}"]["get"]["parameters"]
            .as_array()
            .expect("getTask has parameters");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0]["name"], "id");
        assert_eq!(params[0]["in"], "path");
        // Declared non-required, but OpenAPI mandates path params be required.
        assert_eq!(params[0]["required"], true);
        assert_eq!(params[0]["schema"]["type"], "string");
    }

    #[test]
    fn declared_error_responses_are_emitted() {
        let spec = vocab_to_openapi(sophia_api(), &example_surface()).unwrap();
        let responses = &spec["paths"]["/tasks"]["post"]["responses"];
        assert_eq!(responses["200"]["description"], "The created task");
        assert_eq!(
            responses["400"]["description"],
            "Invalid input (invalid_input)"
        );
    }

    #[test]
    fn servers_are_projected_from_api_server_instances() {
        let spec = vocab_to_openapi(sophia_api(), &example_surface()).unwrap();
        let servers = spec["servers"].as_array().expect("servers present");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["url"], "https://api.sophia-labs.com");
        assert_eq!(servers[0]["description"], "Production");
    }

    #[test]
    fn emitted_spec_serializes_to_valid_json() {
        let spec = vocab_to_openapi(sophia_api(), &example_surface()).unwrap();
        let serialized = serde_json::to_string(&spec).expect("spec serializes");
        // And re-parses — a real, well-formed JSON document.
        let reparsed: Value = serde_json::from_str(&serialized).expect("spec re-parses");
        assert_eq!(reparsed["openapi"], OPENAPI_VERSION);
    }

    #[test]
    fn malformed_schema_literal_is_a_loud_halt() {
        let mut surface = example_surface();
        surface.operations[0].binding.as_mut().unwrap().input_schema =
            Some("{not valid json".to_string());
        let err = vocab_to_openapi(sophia_api(), &surface).unwrap_err();
        assert!(
            err.contains("createTask") && err.contains("not valid JSON"),
            "loud halt names the operation + the problem: {err}"
        );
    }

    #[test]
    fn output_is_deterministic() {
        let surface = example_surface();
        let a = vocab_to_openapi(sophia_api(), &surface).unwrap();
        let b = vocab_to_openapi(sophia_api(), &surface).unwrap();
        // serde_json preserve_order makes the serialized forms byte-equal.
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "the emitter is pure + deterministic"
        );
    }

    #[test]
    fn operation_without_binding_still_emits_a_valid_operation() {
        // An Operation with no WorkflowBinding (no I/O schemas) must still produce a
        // valid operation object with a 200 response — the face never half-emits.
        let surface = ApiSurface {
            operations: vec![ApiOperation {
                operation_id: "ping".to_string(),
                method: "GET".to_string(),
                path: "/ping".to_string(),
                summary: None,
                description: None,
                deprecated: false,
                parameters: vec![],
                responses: vec![],
                binding: None,
            }],
            servers: vec![],
        };
        let spec = vocab_to_openapi(sophia_api(), &surface).unwrap();
        let op = &spec["paths"]["/ping"]["get"];
        assert_eq!(op["operationId"], "ping");
        assert!(op["responses"]["200"].is_object());
        assert!(op.get("requestBody").is_none());
        assert!(op.get("x-workflow-name").is_none());
    }

    #[test]
    fn reads_canonical_workflow_invocation_objects_from_projection_api() {
        let store = Store::new().unwrap();
        let graph_id = "lab";
        let graph = api_projection_graph_iri(graph_id);
        let op = "urn:test:op:createTask";
        let param = "urn:test:param:createTask:title";
        let resp = "urn:test:resp:createTask:200";
        let binding = "urn:test:binding:createTask";
        let server = "urn:test:server:prod";
        let wf = WF_NS;
        let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let xsd_boolean = "http://www.w3.org/2001/XMLSchema#boolean";
        let xsd_integer = "http://www.w3.org/2001/XMLSchema#integer";

        insert_named(&store, &graph, op, rdf_type, iri(&format!("{wf}Operation")));
        insert_named(
            &store,
            &graph,
            op,
            &format!("{wf}operationId"),
            lit("createTask"),
        );
        insert_named(&store, &graph, op, &format!("{wf}method"), lit("POST"));
        insert_named(&store, &graph, op, &format!("{wf}path"), lit("/tasks"));
        insert_named(
            &store,
            &graph,
            op,
            &format!("{wf}summary"),
            lit("Create a task"),
        );
        insert_named(
            &store,
            &graph,
            op,
            &format!("{wf}description"),
            lit("Files a new task into the graph."),
        );
        insert_named(
            &store,
            &graph,
            op,
            &format!("{wf}deprecated"),
            lit_typed("false", xsd_boolean),
        );

        insert_named(
            &store,
            &graph,
            param,
            rdf_type,
            iri(&format!("{wf}Parameter")),
        );
        insert_named(&store, &graph, param, &format!("{wf}ofOperation"), iri(op));
        insert_named(&store, &graph, param, &format!("{wf}name"), lit("title"));
        insert_named(&store, &graph, param, &format!("{wf}in"), lit("query"));
        insert_named(
            &store,
            &graph,
            param,
            &format!("{wf}required"),
            lit_typed("true", xsd_boolean),
        );
        insert_named(
            &store,
            &graph,
            param,
            &format!("{wf}datatype"),
            lit("string"),
        );
        insert_named(
            &store,
            &graph,
            param,
            &format!("{wf}description"),
            lit("The task title"),
        );

        insert_named(
            &store,
            &graph,
            resp,
            rdf_type,
            iri(&format!("{wf}Response")),
        );
        insert_named(&store, &graph, resp, &format!("{wf}ofOperation"), iri(op));
        insert_named(
            &store,
            &graph,
            resp,
            &format!("{wf}statusCode"),
            lit_typed("200", xsd_integer),
        );
        insert_named(
            &store,
            &graph,
            resp,
            &format!("{wf}description"),
            lit("The created task"),
        );

        insert_named(
            &store,
            &graph,
            binding,
            rdf_type,
            iri(&format!("{wf}WorkflowBinding")),
        );
        insert_named(
            &store,
            &graph,
            binding,
            &format!("{wf}bindsOperation"),
            iri(op),
        );
        insert_named(
            &store,
            &graph,
            binding,
            &format!("{wf}workflowName"),
            lit("file-task"),
        );
        insert_named(
            &store,
            &graph,
            binding,
            &format!("{wf}bindsWorkflow"),
            iri("urn:sophia:wf:file-task"),
        );
        insert_named(
            &store,
            &graph,
            binding,
            &format!("{wf}definitionDigest"),
            lit("abc123"),
        );
        insert_named(
            &store,
            &graph,
            binding,
            &format!("{wf}executor"),
            lit("dumb"),
        );
        for (predicate, value) in [
            ("controller", "program"),
            ("modelUse", "inference"),
            ("isolation", "sandbox"),
            ("effects", "capability-bound"),
            ("reproducibility", "nondeterministic"),
            ("runtimeKind", "node"),
            ("minimumRole", "viewer"),
        ] {
            insert_named(
                &store,
                &graph,
                binding,
                &format!("{wf}{predicate}"),
                lit(value),
            );
        }
        insert_named(
            &store,
            &graph,
            binding,
            &format!("{wf}capability"),
            lit("audio.transcribe"),
        );
        insert_named(
            &store,
            &graph,
            binding,
            &format!("{wf}inputSchema"),
            lit(
                r#"{"type":"object","required":["title"],"properties":{"title":{"type":"string"}}}"#,
            ),
        );
        insert_named(
            &store,
            &graph,
            binding,
            &format!("{wf}outputSchema"),
            lit(r#"{"type":"object","properties":{"id":{"type":"string"}}}"#),
        );

        insert_named(
            &store,
            &graph,
            server,
            rdf_type,
            iri(&format!("{wf}Server")),
        );
        insert_named(
            &store,
            &graph,
            server,
            &format!("{wf}url"),
            iri("https://api.sophia-labs.com"),
        );
        insert_named(
            &store,
            &graph,
            server,
            &format!("{wf}description"),
            lit("Production"),
        );

        let surface = read_api_surface(&store, graph_id).expect("wf: surface reads");
        assert_eq!(surface.operations.len(), 1);
        assert_eq!(surface.servers.len(), 1);
        let op = &surface.operations[0];
        assert_eq!(op.operation_id, "createTask");
        assert_eq!(op.method, "POST");
        assert_eq!(op.path, "/tasks");
        assert_eq!(op.parameters.len(), 1);
        assert_eq!(op.parameters[0].name, "title");
        assert_eq!(op.responses.len(), 1);
        assert_eq!(op.responses[0].status_code, 200);
        let binding = op.binding.as_ref().expect("binding reconstructed");
        assert_eq!(binding.workflow_name, "file-task");
        assert_eq!(binding.executor, "dumb");
        assert_eq!(
            binding.workflow_uri.as_deref(),
            Some("urn:sophia:wf:file-task")
        );
        assert_eq!(binding.definition_digest.as_deref(), Some("abc123"));
        assert_eq!(binding.model_use.as_deref(), Some("inference"));
        assert_eq!(binding.capabilities, vec!["audio.transcribe"]);

        let spec = vocab_to_openapi(workflow_vocab(), &surface).expect("workflow OpenAPI emits");
        assert_eq!(spec["info"]["title"], "Mnemosyne Workflow Vocabulary");
        assert_eq!(spec["paths"]["/tasks"]["post"]["operationId"], "createTask");
        assert_eq!(
            spec["paths"]["/tasks"]["post"]["x-workflow-name"],
            "file-task"
        );
        assert_eq!(
            spec["paths"]["/tasks"]["post"]["x-execution"]["modelUse"],
            "inference"
        );
        assert_eq!(spec["servers"][0]["url"], "https://api.sophia-labs.com");
        assert_eq!(
            spec["components"]["schemas"]["createTaskInput"]["properties"]["title"]["type"],
            "string"
        );
    }

    #[test]
    fn contested_executable_binding_fails_closed() {
        let store = Store::new().unwrap();
        let graph_id = "contested";
        let graph = api_projection_graph_iri(graph_id);
        let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        let op = "urn:test:op:run";
        insert_named(
            &store,
            &graph,
            op,
            rdf_type,
            iri(&format!("{WF_NS}Operation")),
        );
        insert_named(
            &store,
            &graph,
            op,
            &format!("{WF_NS}operationId"),
            lit("run"),
        );
        insert_named(&store, &graph, op, &format!("{WF_NS}method"), lit("POST"));
        insert_named(&store, &graph, op, &format!("{WF_NS}path"), lit("/run"));
        for (subject, workflow) in [("urn:test:binding:a", "a"), ("urn:test:binding:b", "b")] {
            insert_named(
                &store,
                &graph,
                subject,
                rdf_type,
                iri(&format!("{WF_NS}WorkflowBinding")),
            );
            insert_named(
                &store,
                &graph,
                subject,
                &format!("{WF_NS}bindsOperation"),
                iri(op),
            );
            insert_named(
                &store,
                &graph,
                subject,
                &format!("{WF_NS}workflowName"),
                lit(workflow),
            );
            insert_named(
                &store,
                &graph,
                subject,
                &format!("{WF_NS}executor"),
                lit("dumb"),
            );
        }

        let error = read_api_surface(&store, graph_id).unwrap_err();
        assert!(error.contains("contested executable binding"), "{error}");
    }
}
