//! HTTP surface for the emporium vocabulary registry, served from inside the
//! gardend cell. Mirrors the platform's `/emporium/vocab*` endpoints so the
//! gateway can stay a blind proxy:
//!
//! - `GET /emporium/vocabs`               — list of `{name,version,namespace,sha,title}`
//! - `GET /emporium/vocab/{name}/latest`  — the contract (latest version)
//! - `GET /emporium/vocab/{name}/{version}` — the contract (exact version)
//!
//! Caching/identity semantics match the platform exactly: `ETag = "{sha}"`,
//! `Cache-Control: public, max-age=300`, `304` when `If-None-Match` matches the
//! sha (after stripping `W/` and quotes), and format negotiation via `?format=`
//! (wins) then `Accept` (default `json`, unknown → `json`).
//!
//! These endpoints are intentionally public (no scope guard), matching the
//! platform where all vocab endpoints are auth-free.

use crate::emporium::contract::{get_vocabulary, VocabularyContract};
use crate::emporium::ingest_routes::ingest_handler;
use crate::emporium::openapi_emit::{
    open_api_store, read_api_surface, vocab_to_openapi, ApiSurface,
};
use crate::emporium::shacl_emit::vocab_to_shacl;
use crate::emporium::vocabs::{all_contracts, find_contract, VocabContract};
use crate::loopback_http::{loopback_error, require_loopback_scope};
use crate::loopback_state::LoopbackState;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;

const CACHE_CONTROL: &str = "public, max-age=300";

/// The empty-for-now emporium router. Phase 1 mounts the vocab registry; later
/// phases will merge survey/ingest sub-routers here. Returns a
/// `Router<Arc<LoopbackState>>` so it drops straight into the loopback merge
/// chain (the vocab routes do not currently read state, but typing it this way
/// keeps the chain uniform and lets later phases reach the cell store).
pub(crate) fn loopback_emporium_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/emporium/sweep/{graph_id}", post(sweep_handler))
        .route("/emporium/heads/{graph_id}", get(heads_handler))
        .route(
            "/emporium/objects/{graph_id}/{vocab}",
            post(objects_create_handler),
        )
        .route(
            "/emporium/objects/{graph_id}/{vocab}/{class}",
            get(objects_list_handler),
        )
        .route(
            "/emporium/objects/{graph_id}/{vocab}/{class}/{address}",
            get(objects_read_handler)
                .put(objects_update_handler)
                .delete(objects_delete_handler),
        )
        .route("/emporium/vocabs", get(list_vocabs))
        .route("/emporium/vocab/{name}/latest", get(get_vocab_latest))
        .route("/emporium/vocab/{name}/{version}", get(get_vocab_version))
        // T2 item 11 (the query face): the emitted named-query CATALOG for one
        // class — names, params, SPARQL text — graph-agnostic (like the vocab
        // faces above), never executed here (execution is the graph-bound
        // `sparql_query_named` MCP tool, see `query_engine.rs`).
        .route(
            "/emporium/vocab/{name}/{class}/query",
            get(get_vocab_class_query),
        )
        // DSL-agnostic ingest endpoint: graph_id is a PATH param (bypasses the
        // camelCase `graphId` sparql seam). Receives + validates + surveys + ACKs;
        // the per-class mint / CRDT applier write path is HELD on the DSL.
        .route("/emporium/ingest/{graph_id}", post(ingest_handler))
        // The POPULATED OpenAPI face (EA-3 Seq 9 / UC-2): a per-graph endpoint that
        // READS the published `api:` instances from `:projection:api` and serves the
        // FULL OpenAPI 3.x spec (not the metadata shell the global, graph-agnostic
        // vocab face emits). graph_id is a PATH param; the surface is graph-bound +
        // requires `rdf.query`, so the contract face stays public + graph-agnostic.
        .route(
            "/g/{graph_id}/vocab/sophia-api/openapi",
            get(get_vocab_openapi_populated),
        )
        .route(
            "/g/{graph_id}/vocab/workflow/openapi",
            get(get_workflow_openapi_populated),
        )
}

#[derive(Debug, Deserialize)]
struct FormatQuery {
    format: Option<String>,
}

/// Negotiated response format. `json` is always available; `turtle` is the derived
/// SHACL face; `markdown` is the curl-friendly human-readable face;
/// `openapi`/`openapi-yaml` are the EA-3 Seq 9 OpenAPI face; `html` stays a Phase-1
/// stub (see `render_body`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VocabFormat {
    Json,
    Turtle,
    Html,
    /// Curl-friendly Markdown rendering of the contract: title, version,
    /// description, jurisdiction, then per-class properties + projection faces.
    Markdown,
    /// OpenAPI 3.x as `application/openapi+json` (the EA-3 Seq 9 face).
    OpenApiJson,
    /// OpenAPI 3.x served as `application/openapi+yaml`. A JSON document is itself
    /// valid YAML, so this serves the same emitted spec under the YAML media type
    /// (no extra serializer dependency).
    OpenApiYaml,
}

/// Resolve the response format: explicit `?format=` wins (unknown → json),
/// else fall back to the `Accept` header (`text/turtle`, `text/markdown`,
/// `text/html`), defaulting to json. Mirrors the platform's `_resolve_format`.
fn resolve_format(headers: &HeaderMap, fmt: Option<&str>) -> VocabFormat {
    if let Some(fmt) = fmt {
        return match fmt.to_ascii_lowercase().as_str() {
            "json" => VocabFormat::Json,
            "turtle" => VocabFormat::Turtle,
            "html" => VocabFormat::Html,
            "markdown" | "md" => VocabFormat::Markdown,
            "openapi" | "openapi-json" => VocabFormat::OpenApiJson,
            "openapi-yaml" | "openapi.yaml" => VocabFormat::OpenApiYaml,
            _ => VocabFormat::Json,
        };
    }
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if accept.contains("application/openapi+yaml") {
        VocabFormat::OpenApiYaml
    } else if accept.contains("application/openapi+json") {
        VocabFormat::OpenApiJson
    } else if accept.contains("text/turtle") {
        VocabFormat::Turtle
    } else if accept.contains("text/markdown") {
        VocabFormat::Markdown
    } else if accept.contains("text/html") {
        VocabFormat::Html
    } else {
        VocabFormat::Json
    }
}

/// True when `If-None-Match` carries the given sha. Strips the `W/` weak
/// validator prefix and surrounding quotes from each comma-listed candidate.
/// Mirrors the platform's `_etag_matches`.
fn etag_matches(headers: &HeaderMap, sha: &str) -> bool {
    let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    if inm.is_empty() {
        return false;
    }
    inm.split(',').any(|candidate| {
        let candidate = candidate.trim();
        let candidate = candidate.strip_prefix("W/").unwrap_or(candidate);
        candidate.trim_matches('"') == sha
    })
}

/// Render the contract body for a negotiated format. JSON is REQUIRED and is
/// the verbatim embedded bytes. The Turtle face (EA-3) is the vocab's SHACL
/// VALIDATION SHAPES — the exact shapes the generic ingest gate validates
/// against, made browsable: the thin contract is resolved to its parsed twin via
/// `get_vocabulary(name)` (the B7 thin→parsed seam) and run through the pure,
/// deterministic `vocab_to_shacl`. A thin-only miss (a contract with no parsed
/// registration) falls back to the honest placeholder. HTML stays a Phase-1 stub.
fn render_body(contract: &VocabContract, format: VocabFormat) -> (String, &'static str) {
    match format {
        VocabFormat::Json => (contract.json.to_string(), "application/json"),
        // The Turtle face = the contract's derived SHACL shapes (EA-3). Resolve the
        // parsed twin (B7 seam); on a hit, emit the real shapes — the same Turtle
        // the validator round-trips through rudof.
        VocabFormat::Turtle => match get_vocabulary(contract.name) {
            Some(parsed) => (vocab_to_shacl(parsed), "text/turtle"),
            None => (
                format!(
                    "# emporium vocab '{}' v{} has no parsed contract registered;\n\
                     # SHACL shapes unavailable. Canonical JSON is at ?format=json.\n\
                     # sha: {}\n",
                    contract.name, contract.version, contract.sha
                ),
                "text/turtle",
            ),
        },
        VocabFormat::Html => (
            format!(
                "<!doctype html><meta charset=\"utf-8\"><title>{name} v{version}</title>\
                 <p>emporium vocab HTML rendering not yet ported (Phase 2+).</p>\
                 <p>vocabulary: {name} v{version}, sha {sha}. \
                 Canonical JSON is available at <code>?format=json</code>.</p>",
                name = contract.name,
                version = contract.version,
                sha = contract.sha
            ),
            "text/html",
        ),
        // The Markdown face: a curl-friendly rendering of the contract. Resolves
        // the parsed twin (B7 seam) exactly like the Turtle/OpenAPI faces; a
        // thin-only miss falls back to the same honest placeholder pattern.
        VocabFormat::Markdown => render_markdown(contract),
        // The OpenAPI face (EA-3 Seq 9): resolve the parsed twin (B7 seam) and emit
        // a valid OpenAPI 3.x document via the pure `vocab_to_openapi` materializer.
        // The SURFACE (the published api: operation instances in `:projection:api`)
        // is read from the graph by the LIVE caller (Seq 10 / CA-1) and is empty
        // here — `render_body` is a pure rendering helper with no graph access — so
        // this serves the metadata-shell OpenAPI document (valid 3.x: info + empty
        // paths + empty components). The materializer itself is proven over a full
        // real surface in `openapi_emit::tests`. A thin-only miss (no parsed
        // contract) falls back to the honest placeholder, matching the Turtle face.
        VocabFormat::OpenApiJson => render_openapi(contract, false),
        VocabFormat::OpenApiYaml => render_openapi(contract, true),
    }
}

/// Render the OpenAPI face for a contract: resolve its parsed twin, emit the spec
/// over the (deferred) graph surface, and serialize. `yaml` selects the
/// `application/openapi+yaml` media type — a JSON document is valid YAML, so the
/// same serialized bytes are served under the YAML type (no extra dependency).
fn render_openapi(contract: &VocabContract, yaml: bool) -> (String, &'static str) {
    let media = if yaml {
        "application/openapi+yaml"
    } else {
        "application/openapi+json"
    };
    match get_vocabulary(contract.name) {
        Some(parsed) => {
            // Empty surface: the operation instances live in `:projection:api` and
            // are read by the LIVE caller (Seq 10 / CA-1), not by this pure helper.
            let surface = ApiSurface::default();
            match vocab_to_openapi(parsed, &surface) {
                Ok(spec) => (
                    serde_json::to_string_pretty(&spec)
                        .unwrap_or_else(|e| format!("{{\"error\":\"openapi serialize failed: {e}\"}}")),
                    media,
                ),
                // A loud halt here would only fire on a malformed embedded schema
                // literal; the empty served surface carries none, so this is inert
                // today but kept honest rather than `.unwrap()`.
                Err(e) => (
                    format!("{{\"error\":\"openapi emit failed: {e}\"}}"),
                    "application/json",
                ),
            }
        }
        None => (
            format!(
                "{{\"error\": \"emporium vocab '{}' v{} has no parsed contract; OpenAPI unavailable\"}}",
                contract.name, contract.version
            ),
            "application/json",
        ),
    }
}

/// Render the Markdown face for a contract: title, version, description,
/// jurisdiction, then per class its properties (datatype + cardinality) and
/// `projection_faces` (when declared). Curl-friendly — the intended reader is a
/// human (or an agent) piping `curl .../latest?format=markdown` straight to a
/// terminal. Resolves the parsed twin (B7 seam) exactly like the Turtle/OpenAPI
/// faces above; a thin-only miss (no parsed contract registered) falls back to
/// the same honest placeholder pattern, built from the thin [`VocabContract`]
/// metadata alone.
fn render_markdown(contract: &VocabContract) -> (String, &'static str) {
    match get_vocabulary(contract.name) {
        Some(parsed) => (markdown_from_parsed(contract, parsed), "text/markdown"),
        None => (
            format!(
                "# {title}\n\n\
                 **version:** {version}\n\
                 **jurisdiction:** {jurisdiction}\n\n\
                 _emporium vocab '{name}' v{version} has no parsed contract registered; \
                 markdown detail unavailable. Canonical JSON is at `?format=json`._\n\n\
                 _sha: {sha}_\n",
                title = markdown_escape(contract.title),
                version = markdown_escape(contract.version),
                jurisdiction = markdown_escape(contract.public_jurisdiction),
                name = contract.name,
                sha = contract.sha
            ),
            "text/markdown",
        ),
    }
}

/// Build the Markdown body from the resolved parsed contract. `classes` and each
/// class's `predicates` are `BTreeMap`s, so the rendering order is deterministic
/// (stable for tests/snapshots) without an explicit sort.
fn markdown_from_parsed(contract: &VocabContract, parsed: &VocabularyContract) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", markdown_escape(&parsed.title)));
    out.push_str(&format!(
        "**version:** {}\n",
        markdown_escape(&parsed.version)
    ));
    out.push_str(&format!(
        "**jurisdiction:** {}\n\n",
        markdown_escape(contract.public_jurisdiction)
    ));
    if !parsed.description.is_empty() {
        out.push_str(&format!("{}\n\n", markdown_escape(&parsed.description)));
    }

    for (class_name, class_spec) in &parsed.classes {
        out.push_str(&format!("## {}\n\n", markdown_escape(class_name)));
        if let Some(comment) = class_spec.comment.as_deref().filter(|c| !c.is_empty()) {
            out.push_str(&format!("{}\n\n", markdown_escape(comment)));
        }

        if class_spec.predicates.is_empty() {
            out.push_str("_no declared properties_\n\n");
        } else {
            for (predicate_name, predicate_spec) in &class_spec.predicates {
                let cardinality = match (predicate_spec.required, predicate_spec.multi) {
                    (true, true) => "1..*",
                    (true, false) => "1..1",
                    (false, true) => "0..*",
                    (false, false) => "0..1",
                };
                out.push_str(&format!(
                    "- `{}` — datatype `{:?}`, cardinality `{}`\n",
                    markdown_escape(predicate_name),
                    predicate_spec.datatype,
                    cardinality
                ));
            }
            out.push('\n');
        }

        if !class_spec.projection_faces.is_empty() {
            let faces = class_spec
                .projection_faces
                .iter()
                .map(|face| format!("`{}`", markdown_escape(face)))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("**projection_faces:** {faces}\n\n"));
        }
    }

    out
}

/// Escape a contract-authored string for safe embedding in the Markdown face.
/// Contract text (title/description/comment/class+property names) is baked
/// into the served binary today, but nothing here should assume it always will
/// be — so this backslash-escapes the CommonMark punctuation that would
/// otherwise be parsed as syntax rather than rendered literally: backtick
/// (would close an inline code span early), `*`/`_` (emphasis), `[`/`]` (link
/// syntax), `<`/`>` (inline HTML / autolink), and `|` (breaks out of the
/// bullet lines this renderer emits, and would break a table cell if the
/// layout ever grows one).
fn markdown_escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// `GET /g/{graph_id}/vocab/sophia-api/openapi` — the compatibility POPULATED
/// OpenAPI face.
///
/// Reads the published `api:` instances from this graph's `:projection:api` sink,
/// reconstructs the [`ApiSurface`] (via `read_api_surface`), and serves the full
/// OpenAPI 3.x spec (via the pure `vocab_to_openapi` materializer). UNLIKE the
/// global `/emporium/vocab/sophia-api/latest?format=openapi` face — which is
/// graph-agnostic + pure, so it can only emit the metadata SHELL — this endpoint
/// has store access and serves the REAL published operations.
///
/// `?format=openapi-yaml` (or `Accept: application/openapi+yaml`) selects the YAML
/// media type (a JSON document is valid YAML, so the same bytes are served). The
/// graph-bound surface requires the `rdf.query` scope (read-only).
///
/// A graph with no published operations is NOT an error — it serves a valid
/// empty-paths OpenAPI document. The LIVE endpoint (executing the bound workflow
/// via Choreograph) is CA-1's, gated on CA-1's WF-4 — explicitly OUT of scope.
async fn get_vocab_openapi_populated(
    Path(graph_id): Path<String>,
    Query(query): Query<FormatQuery>,
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    // Read-only scope: this path NEVER writes nor invokes a workflow.
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.query") {
        return response;
    }

    // Open the per-graph store (resolve the graph dir; 404 a missing graph), then
    // delegate to the shared serving logic over the real store.
    let store = match open_api_store(&state.app, &graph_id) {
        Ok(store) => store,
        Err(error) => {
            return loopback_error(
                StatusCode::NOT_FOUND,
                &format!("graph '{graph_id}': {error}"),
            )
        }
    };
    populated_openapi_response(&store, &graph_id, query.format.as_deref(), &headers)
}

/// `GET /g/{graph_id}/vocab/workflow/openapi` — the canonical Workflow-owned
/// POPULATED OpenAPI face.
async fn get_workflow_openapi_populated(
    Path(graph_id): Path<String>,
    Query(query): Query<FormatQuery>,
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    // Read-only scope: this path NEVER writes nor invokes a workflow.
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.query") {
        return response;
    }

    let store = match open_api_store(&state.app, &graph_id) {
        Ok(store) => store,
        Err(error) => {
            return loopback_error(
                StatusCode::NOT_FOUND,
                &format!("graph '{graph_id}': {error}"),
            )
        }
    };
    populated_workflow_openapi_response(&store, &graph_id, query.format.as_deref(), &headers)
}

/// The shared, store-driven serving logic for the POPULATED OpenAPI face: read the
/// published surface from `:projection:api`, materialize the spec, negotiate the
/// media type, attach a CONTENT-derived ETag + Cache-Control, honor `If-None-Match`
/// (304). Sync over a `&Store` so the route handler (after the scope check + store
/// open) AND the end-to-end oracle exercise the IDENTICAL serving path.
pub(crate) fn populated_openapi_response(
    store: &oxigraph::store::Store,
    graph_id: &str,
    fmt: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    populated_openapi_response_for_vocab(store, graph_id, "sophia-api", fmt, headers)
}

/// Canonical Workflow-owned OpenAPI serving helper. Kept beside
/// [`populated_openapi_response`] so the old `sophia-api` oracle remains a
/// compatibility proof while new callers can address the invocation face through
/// Workflow.
pub(crate) fn populated_workflow_openapi_response(
    store: &oxigraph::store::Store,
    graph_id: &str,
    fmt: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    populated_openapi_response_for_vocab(store, graph_id, "workflow", fmt, headers)
}

fn populated_openapi_response_for_vocab(
    store: &oxigraph::store::Store,
    graph_id: &str,
    vocab_name: &str,
    fmt: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    // The contract (its metadata seeds info/title/version + the namespace bindings).
    let Some(contract) = find_contract(vocab_name, "latest") else {
        return loopback_error(
            StatusCode::NOT_FOUND,
            &format!("vocabulary '{vocab_name}' not found"),
        );
    };
    let Some(parsed) = get_vocabulary(contract.name) else {
        return loopback_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(
                "emporium vocab '{}' has no parsed contract; OpenAPI unavailable",
                contract.name
            ),
        );
    };

    // Read the published surface from :projection:api (the POPULATED half).
    let surface = match read_api_surface(store, graph_id) {
        Ok(surface) => surface,
        Err(error) => {
            return loopback_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("read api surface: {error}"),
            )
        }
    };

    // Materialize the POPULATED spec (a malformed published schema literal is a loud
    // halt from the materializer — surfaced as a 422, not a half-formed spec).
    let spec = match vocab_to_openapi(parsed, &surface) {
        Ok(spec) => spec,
        Err(error) => {
            return loopback_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("openapi emit failed: {error}"),
            )
        }
    };

    let yaml = matches!(resolve_format(headers, fmt), VocabFormat::OpenApiYaml);
    let media = if yaml {
        "application/openapi+yaml"
    } else {
        "application/openapi+json"
    };
    let body = serde_json::to_string_pretty(&spec)
        .unwrap_or_else(|e| format!("{{\"error\":\"openapi serialize failed: {e}\"}}"));

    // CONTENT-derived ETag: the populated spec varies by graph + published surface,
    // so the contract sha alone would be a lie (two different surfaces sharing one
    // validator). Hash the served body so `If-None-Match` is honest. Honor a match
    // with a 304 before re-serving.
    let etag_sha = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    if etag_matches(headers, &etag_sha) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, format!("\"{etag_sha}\"")),
                (header::CACHE_CONTROL, CACHE_CONTROL.to_string()),
            ],
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, media.to_string()),
            (header::ETAG, format!("\"{etag_sha}\"")),
            (header::CACHE_CONTROL, CACHE_CONTROL.to_string()),
        ],
        body,
    )
        .into_response()
}

/// `GET /emporium/vocabs` — list all registered vocabularies (latest of each).
/// `POST /emporium/sweep/{graph_id}` — the §8.1 post-hoc conformance sweep:
/// contested-lineage detection over the LIVE memory projection, findings filed
/// to the violation ledger as the sweep observer's advisory testimony.
/// `?observer=` selects a per-agent membrane; default = the shared commons.
/// Requires `rdf.update` (it writes ledger testimony).
async fn sweep_handler(
    Path(graph_id): Path<String>,
    Query(query): Query<SweepQuery>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.update") {
        return response;
    }
    // Recorded in the source ledger on a graph under source authority
    // (`source_sync::record_authored_write`); unchanged elsewhere.
    let swept = crate::source_sync::record_authored_write(
        &state.app,
        &graph_id,
        "emporiumSweep",
        crate::source_sync::AuthoredScope::Authored,
        async {
            crate::emporium::sweep::sweep_memory_conformance(
                &state.app,
                &graph_id,
                query.observer.as_deref().unwrap_or(""),
            )
        },
    )
    .await;
    match swept {
        Ok(report) => (StatusCode::OK, Json(serde_json::json!(report))).into_response(),
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

#[derive(Deserialize)]
struct SweepQuery {
    observer: Option<String>,
}

/// `GET /emporium/heads/{graph_id}` — the ratified "return all heads flagged"
/// read: every current memory head, grouped by lineage, each lineage flagged
/// `contested` when it has >1 head. A pure read (rdf.query); storage never picks
/// a winner. `?observer=` selects a per-agent membrane; default = the commons.
async fn heads_handler(
    Path(graph_id): Path<String>,
    Query(query): Query<SweepQuery>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.query") {
        return response;
    }
    match crate::emporium::sweep::current_heads_flagged(
        &state.app,
        &graph_id,
        query.observer.as_deref().unwrap_or(""),
    ) {
        Ok(report) => (StatusCode::OK, Json(serde_json::json!(report))).into_response(),
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

// ── the generic Meaningful-Object CRUD surface (see emporium/objects.rs) ──
// Thin route shims: scope check → objects fn → status-mapped JSON. Reads take
// rdf.query; every mutation takes rdf.update (and the objects layer holds the
// per-graph write gate).

fn object_error_response(e: crate::emporium::objects::ObjectError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    loopback_error(status, e.message())
}

/// Run one object mutation; on a graph under source authority its effect is
/// recorded in the source ledger (`source_sync::record_authored_write`), so a
/// rebuild replays it. Unchanged elsewhere.
async fn recorded_object_response(
    state: &Arc<LoopbackState>,
    graph_id: &str,
    origin: &'static str,
    mutation: impl std::future::Future<
        Output = Result<serde_json::Value, crate::emporium::objects::ObjectError>,
    >,
) -> Response {
    let recorded = crate::source_sync::record_authored_write(
        &state.app,
        graph_id,
        origin,
        crate::source_sync::AuthoredScope::Authored,
        async { Ok::<_, crate::app_error::AppError>(mutation.await) },
    )
    .await;
    match recorded {
        Ok(Ok(body)) => (StatusCode::OK, Json(body)).into_response(),
        Ok(Err(e)) => object_error_response(e),
        Err(error) => crate::loopback_http::loopback_app_error(error),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn objects_list_handler(
    Path((graph_id, vocab, class)): Path<(String, String, String)>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.query") {
        return response;
    }
    match crate::emporium::objects::list_objects(
        &state.app,
        &graph_id,
        &vocab,
        &class,
        query.limit.unwrap_or(100),
        query.offset.unwrap_or(0),
    ) {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(e) => object_error_response(e),
    }
}

async fn objects_read_handler(
    Path((graph_id, vocab, class, address)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.query") {
        return response;
    }
    match crate::emporium::objects::read_object(&state.app, &graph_id, &vocab, &class, &address) {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(e) => object_error_response(e),
    }
}

async fn objects_create_handler(
    Path((graph_id, vocab)): Path<(String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.update") {
        return response;
    }
    let records = body.get("records").cloned().unwrap_or(body);
    recorded_object_response(
        &state,
        &graph_id,
        "emporiumObjectCreate",
        crate::emporium::objects::create_objects(&state.app, &graph_id, &vocab, records),
    )
    .await
}

async fn objects_update_handler(
    Path((graph_id, vocab, class, address)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
    Json(record): Json<serde_json::Value>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.update") {
        return response;
    }
    recorded_object_response(
        &state,
        &graph_id,
        "emporiumObjectUpdate",
        crate::emporium::objects::update_object(
            &state.app, &graph_id, &vocab, &class, &address, record,
        ),
    )
    .await
}

async fn objects_delete_handler(
    Path((graph_id, vocab, class, address)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "rdf.update") {
        return response;
    }
    recorded_object_response(
        &state,
        &graph_id,
        "emporiumObjectDelete",
        crate::emporium::objects::delete_object(&state.app, &graph_id, &vocab, &class, &address),
    )
    .await
}

async fn list_vocabs() -> Json<serde_json::Value> {
    let vocabularies: Vec<serde_json::Value> = all_contracts()
        .into_iter()
        .map(|contract| {
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
            })
        })
        .collect();
    Json(serde_json::json!({ "vocabularies": vocabularies }))
}

async fn get_vocab_latest(
    Path(name): Path<String>,
    Query(query): Query<FormatQuery>,
    headers: HeaderMap,
) -> Response {
    vocab_response(&name, "latest", query.format.as_deref(), &headers)
}

async fn get_vocab_version(
    Path((name, version)): Path<(String, String)>,
    Query(query): Query<FormatQuery>,
    headers: HeaderMap,
) -> Response {
    vocab_response(&name, &version, query.format.as_deref(), &headers)
}

/// Shared serving logic for `/latest` and `/{version}`: resolve the contract,
/// negotiate format, honor `If-None-Match` (304), and attach `ETag` +
/// `Cache-Control` to every response.
fn vocab_response(
    name: &str,
    version_or_latest: &str,
    fmt: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    let Some(contract) = find_contract(name, version_or_latest) else {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"detail":"vocabulary not found"}"#,
        )
            .into_response();
    };

    let etag_value = format!("\"{}\"", contract.sha);

    if etag_matches(headers, contract.sha) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag_value),
                (header::CACHE_CONTROL, CACHE_CONTROL.to_string()),
            ],
        )
            .into_response();
    }

    let format = resolve_format(headers, fmt);
    let (body, media_type) = render_body(&contract, format);

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, media_type.to_string()),
            (header::ETAG, etag_value),
            (header::CACHE_CONTROL, CACHE_CONTROL.to_string()),
        ],
        body,
    )
        .into_response()
}

/// `GET /emporium/vocab/{name}/{class}/query` — the T2 (item 11) query-face
/// CATALOG: the four canonical named query plans (`byId`/`currentHeads`/
/// `lineageOf`/`countBy`) DERIVED from the class's declared shape +
/// `MaterializationSignature`, never hand-written. Graph-agnostic (like the
/// contract faces above) — this serves the PLAN (names, params, SPARQL text),
/// never executes it; execution is the graph-bound `sparql_query_named` MCP
/// tool. Public, read-only, JSON (default) + Markdown faces via `?format=`/
/// `Accept`, mirroring `resolve_format`'s existing negotiation.
async fn get_vocab_class_query(
    Path((name, class)): Path<(String, String)>,
    Query(query): Query<FormatQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(contract) = get_vocabulary(&name) else {
        return loopback_error(
            StatusCode::NOT_FOUND,
            &format!("vocabulary '{name}' not found"),
        );
    };
    let catalog = match crate::emporium::query_emit::emit_query_catalog(contract, &class) {
        Ok(catalog) => catalog,
        Err(error) => {
            let status =
                StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return loopback_error(status, error.message());
        }
    };

    match resolve_format(&headers, query.format.as_deref()) {
        VocabFormat::Markdown => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/markdown")],
            render_query_catalog_markdown(&catalog),
        )
            .into_response(),
        _ => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            Json(serde_json::json!(catalog)),
        )
            .into_response(),
    }
}

/// Curl-friendly Markdown rendering of a [`crate::emporium::query_emit::QueryCatalog`]:
/// one section per named query, its params, and its SPARQL template — an
/// `unsupported` plan says so instead of showing a template that would just
/// reject at call time.
fn render_query_catalog_markdown(catalog: &crate::emporium::query_emit::QueryCatalog) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} / {} — query catalog\n\n",
        markdown_escape(&catalog.vocab),
        markdown_escape(&catalog.class)
    ));
    out.push_str(&format!(
        "**membraneAware:** {}\n\n",
        catalog.membrane_aware
    ));
    for plan in &catalog.queries {
        out.push_str(&format!("## {}\n\n", markdown_escape(plan.name)));
        out.push_str(&format!("{}\n\n", markdown_escape(&plan.description)));
        if let Some(reason) = &plan.unsupported {
            out.push_str(&format!(
                "_unsupported for this class: {}_\n\n",
                markdown_escape(reason)
            ));
            continue;
        }
        if !plan.params.is_empty() {
            out.push_str("**params:**\n\n");
            for param in &plan.params {
                out.push_str(&format!(
                    "- `{}` ({}): {}\n",
                    markdown_escape(param.name),
                    if param.required {
                        "required"
                    } else {
                        "optional"
                    },
                    markdown_escape(&param.description)
                ));
            }
            out.push('\n');
        }
        out.push_str("```sparql\n");
        out.push_str(&plan.sparql_template);
        out.push_str("\n```\n\n");
    }
    out
}

/// Curl-friendly Markdown rendering of an
/// [`crate::emporium::object_query::ObjectQueryOutcome`] (T2 item 11a's
/// object-query MCP face, `face: "markdown"`). Every VALUE that could carry
/// arbitrary, agent/user-authored content (subject IRIs are the one
/// exception: URI syntax cannot contain a literal backtick) is rendered as
/// ESCAPED PLAIN TEXT, never wrapped in a backtick code span — a code span's
/// backtick delimiter is NOT subject to backslash-escaping in CommonMark, so
/// escaping-then-wrapping would still let an embedded backtick in real field
/// content (e.g. a memory record whose content quotes a shell command) break
/// out of the span. `markdown_escape` alone, on unwrapped text, is sufficient
/// and correct.
pub(crate) fn render_object_query_markdown(
    outcome: &crate::emporium::object_query::ObjectQueryOutcome,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} / {} — object query\n\n",
        markdown_escape(&outcome.vocab),
        markdown_escape(&outcome.class)
    ));
    out.push_str(&format!(
        "**graphId:** `{}`\n",
        markdown_escape(&outcome.graph_id)
    ));
    out.push_str(&format!(
        "**perspectives:** {}\n",
        markdown_escape(&outcome.scope.perspectives)
    ));
    out.push_str(&format!(
        "**scopeGraphs:** {}\n",
        outcome
            .scope
            .graphs
            .iter()
            .map(|g| format!("`{}`", markdown_escape(g)))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "**totalMatched:** {}  **limit:** {}  **offset:** {}\n\n",
        outcome.total_matched, outcome.limit, outcome.offset
    ));

    if !outcome.warnings.is_empty() {
        out.push_str("**warnings:**\n\n");
        for warning in &outcome.warnings {
            out.push_str(&format!("- {}\n", markdown_escape(warning)));
        }
        out.push('\n');
    }

    if outcome.objects.is_empty() {
        out.push_str("_no objects in this page_\n");
    }
    for object in &outcome.objects {
        out.push_str(&format!("## {}\n\n", markdown_escape(&object.subject)));
        out.push_str(&format!(
            "- rdfType: {}\n",
            markdown_escape(&object.rdf_type)
        ));
        if let Some(contested) = object.contested {
            out.push_str(&format!("- contested: {contested}\n"));
            if !object.sibling_heads.is_empty() {
                out.push_str(&format!(
                    "- siblingHeads: {}\n",
                    object
                        .sibling_heads
                        .iter()
                        .map(|s| markdown_escape(s))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        if let Some(witness_graph) = &object.witness_graph {
            out.push_str(&format!(
                "- witnessGraph: {}\n",
                markdown_escape(witness_graph)
            ));
        }
        out.push_str("- predicates:\n");
        for (predicate, values) in &object.predicates {
            let rendered = values
                .iter()
                .map(|v| markdown_escape(v))
                .collect::<Vec<_>>()
                .join("; ");
            out.push_str(&format!(
                "  - {}: {}\n",
                markdown_escape(predicate),
                rendered
            ));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, value.parse().unwrap());
        headers
    }

    #[test]
    fn resolve_format_prefers_query_then_accept() {
        let empty = HeaderMap::new();
        assert_eq!(resolve_format(&empty, Some("turtle")), VocabFormat::Turtle);
        assert_eq!(resolve_format(&empty, Some("HTML")), VocabFormat::Html);
        // Unknown explicit format collapses to json (NOT openapi — the default
        // never becomes the OpenAPI face by accident).
        assert_eq!(resolve_format(&empty, Some("yaml")), VocabFormat::Json);
        // No query → fall back to Accept.
        let turtle_accept = headers_with(header::ACCEPT, "text/turtle");
        assert_eq!(resolve_format(&turtle_accept, None), VocabFormat::Turtle);
        let html_accept = headers_with(header::ACCEPT, "text/html");
        assert_eq!(resolve_format(&html_accept, None), VocabFormat::Html);
        // Default json.
        assert_eq!(resolve_format(&empty, None), VocabFormat::Json);
        // Query wins over Accept.
        assert_eq!(
            resolve_format(&turtle_accept, Some("json")),
            VocabFormat::Json
        );
    }

    #[test]
    fn resolve_format_negotiates_the_openapi_face() {
        let empty = HeaderMap::new();
        // Explicit query.
        assert_eq!(
            resolve_format(&empty, Some("openapi")),
            VocabFormat::OpenApiJson
        );
        assert_eq!(
            resolve_format(&empty, Some("openapi-json")),
            VocabFormat::OpenApiJson
        );
        assert_eq!(
            resolve_format(&empty, Some("openapi-yaml")),
            VocabFormat::OpenApiYaml
        );
        // Accept header.
        let oa_json = headers_with(header::ACCEPT, "application/openapi+json");
        assert_eq!(resolve_format(&oa_json, None), VocabFormat::OpenApiJson);
        let oa_yaml = headers_with(header::ACCEPT, "application/openapi+yaml");
        assert_eq!(resolve_format(&oa_yaml, None), VocabFormat::OpenApiYaml);
        // Query still wins over Accept.
        assert_eq!(resolve_format(&oa_json, Some("json")), VocabFormat::Json);
    }

    #[test]
    fn resolve_format_negotiates_the_markdown_face() {
        let empty = HeaderMap::new();
        // Explicit query, both accepted spellings.
        assert_eq!(
            resolve_format(&empty, Some("markdown")),
            VocabFormat::Markdown
        );
        assert_eq!(resolve_format(&empty, Some("MD")), VocabFormat::Markdown);
        // Accept header.
        let md_accept = headers_with(header::ACCEPT, "text/markdown");
        assert_eq!(resolve_format(&md_accept, None), VocabFormat::Markdown);
        // Query still wins over Accept.
        assert_eq!(resolve_format(&md_accept, Some("json")), VocabFormat::Json);
    }

    #[test]
    fn etag_matches_strips_weak_prefix_and_quotes() {
        let sha = "bc50e854bd4eed51ef3e1644db72662610af34f037d9802f1804a009eec77eff";
        assert!(etag_matches(
            &headers_with(header::IF_NONE_MATCH, &format!("\"{sha}\"")),
            sha
        ));
        assert!(etag_matches(
            &headers_with(header::IF_NONE_MATCH, &format!("W/\"{sha}\"")),
            sha
        ));
        // Bare sha (no quotes) still matches.
        assert!(etag_matches(&headers_with(header::IF_NONE_MATCH, sha), sha));
        // Comma list with the sha somewhere inside.
        assert!(etag_matches(
            &headers_with(header::IF_NONE_MATCH, &format!("\"other\", \"{sha}\"")),
            sha
        ));
        // Non-matching and empty.
        assert!(!etag_matches(
            &headers_with(header::IF_NONE_MATCH, "\"deadbeef\""),
            sha
        ));
        assert!(!etag_matches(&HeaderMap::new(), sha));
    }

    #[test]
    fn render_body_serves_json_verbatim() {
        let contract = crate::emporium::vocabs::workflow_contract();
        let (body, media) = render_body(&contract, VocabFormat::Json);
        assert_eq!(media, "application/json");
        assert_eq!(body, contract.json);
    }

    #[test]
    fn turtle_face_emits_derived_shacl_shapes() {
        // The EA-3 Turtle face renders the vocab's derived SHACL shapes (the same
        // shapes the generic ingest gate validates against), not a stub.
        let contract =
            crate::emporium::vocabs::find_contract("emporium-bookmark", "latest").unwrap();
        let (body, media) = render_body(&contract, VocabFormat::Turtle);
        assert_eq!(media, "text/turtle");
        assert!(
            body.contains("urn:sophia:shacl:emporium-bookmark#BookmarkShape"),
            "turtle face must emit the Bookmark NodeShape: {body}"
        );
        assert!(
            body.contains("a sh:NodeShape"),
            "real SHACL, not a placeholder"
        );
        assert!(
            body.contains("sh:targetClass <http://mnemosyne.dev/bookmark#Bookmark>"),
            "the shape targets bm:Bookmark"
        );
        // The required bm:url predicate gets a minCount.
        assert!(body.contains("sh:path <http://mnemosyne.dev/bookmark#url>"));
        assert!(body.contains("sh:minCount 1"));
    }

    #[test]
    fn markdown_face_renders_a_real_embedded_pack() {
        // The `emporium-bookmark` pack is a real, registered, served product vocab
        // (see turtle_face_emits_derived_shacl_shapes above) — not a fixture built
        // for this test — with one class and a mix of required/optional/multi
        // properties and no declared projection_faces.
        let contract =
            crate::emporium::vocabs::find_contract("emporium-bookmark", "latest").unwrap();
        let (body, media) = render_body(&contract, VocabFormat::Markdown);
        assert_eq!(media, "text/markdown");

        // Title, version, jurisdiction, description.
        assert!(
            body.contains("# Emporium Bookmark Example Vocabulary"),
            "title heading present: {body}"
        );
        assert!(body.contains("**version:** 1.0.0"));
        assert!(body.contains("**jurisdiction:** internal-substrate"));
        assert!(
            body.contains("A minimal example vocabulary: the Bookmark"),
            "contract description rendered: {body}"
        );

        // One class heading.
        assert!(
            body.contains("## Bookmark"),
            "class heading present: {body}"
        );

        // Property lines: datatype + cardinality, required vs optional vs multi.
        assert!(
            body.contains("- `bm:url` — datatype `uri`, cardinality `1..1`"),
            "required scalar property line: {body}"
        );
        assert!(
            body.contains("- `bm:note` — datatype `string`, cardinality `0..1`"),
            "optional scalar property line: {body}"
        );
        assert!(
            body.contains("- `bm:tag` — datatype `string`, cardinality `0..*`"),
            "optional multi-valued property line: {body}"
        );

        // No projection_faces declared on this pack → no such section rendered.
        assert!(!body.contains("projection_faces"));
    }

    #[test]
    fn markdown_face_renders_projection_faces_when_declared() {
        // The `workflow` pack declares projection_faces on several classes.
        let contract = crate::emporium::vocabs::workflow_contract();
        let (body, media) = render_body(&contract, VocabFormat::Markdown);
        assert_eq!(media, "text/markdown");
        assert!(body.contains("# Mnemosyne Workflow Vocabulary"));
        assert!(body.contains("**projection_faces:**"), "{body}");
    }

    #[test]
    fn markdown_face_falls_back_to_placeholder_for_a_thin_only_contract() {
        // Mirrors the Turtle/OpenAPI thin-only-miss fallback pattern: a contract
        // name `get_vocabulary` doesn't resolve still serves a well-formed,
        // honest Markdown placeholder rather than panicking or emitting nothing.
        let contract = crate::emporium::vocabs::VocabContract {
            name: "not-a-registered-vocab",
            version: "0.0.0",
            namespace: "urn:example:",
            title: "Unregistered Example",
            sha: "deadbeef",
            public_jurisdiction: "internal-substrate",
            canonical_ontology: "internal-substrate",
            canonical_pack: "not-a-registered-vocab",
            registry_status: "internal-substrate",
            slug_aliases: &[],
            compatibility_aliases: &[],
            json: "{}",
        };
        let (body, media) = render_body(&contract, VocabFormat::Markdown);
        assert_eq!(media, "text/markdown");
        assert!(body.contains("# Unregistered Example"));
        assert!(body.contains("has no parsed contract registered"));
        assert!(body.contains("?format=json"));
    }

    #[test]
    fn markdown_escape_neutralizes_commonmark_punctuation() {
        assert_eq!(
            markdown_escape("a `code` *bold* _em_ [link](x) <tag> a|b"),
            r"a \`code\` \*bold\* \_em\_ \[link\](x) \<tag\> a\|b"
        );
        // Plain prose round-trips untouched.
        assert_eq!(markdown_escape("plain text 123"), "plain text 123");
    }

    #[test]
    fn vocab_response_serves_markdown_via_content_negotiation() {
        // End-to-end through the shared serving logic: ?format=markdown → 200
        // with the markdown media type + ETag/Cache-Control, and via Accept too.
        let response = vocab_response(
            "emporium-bookmark",
            "latest",
            Some("markdown"),
            &HeaderMap::new(),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/markdown"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            CACHE_CONTROL
        );

        let accept = headers_with(header::ACCEPT, "text/markdown");
        let via_accept = vocab_response("emporium-bookmark", "latest", None, &accept);
        assert_eq!(via_accept.status(), StatusCode::OK);
        assert_eq!(
            via_accept
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/markdown"
        );
    }

    #[test]
    fn turtle_face_emits_world_runtime_shapes_with_kind_to_rdftype_asymmetry() {
        // EA-3 §WS3: the world-runtime pack's Turtle face derives a SHACL shape per
        // class. The shape IRI is named off the class KEY (the wire `kind`) while the
        // sh:targetClass is the DIFFERING rdf_type (SessionMessage → agt:Message).
        let contract =
            crate::emporium::vocabs::find_contract("wf-agent-world-runtime", "latest").unwrap();
        let (body, media) = render_body(&contract, VocabFormat::Turtle);
        assert_eq!(media, "text/turtle");
        assert!(
            body.contains("urn:sophia:shacl:wf-agent-world-runtime#SessionMessageShape"),
            "world-runtime turtle face must emit the SessionMessage NodeShape: {body}"
        );
        assert!(
            body.contains("a sh:NodeShape"),
            "real SHACL, not a placeholder"
        );
        // The class KEY is SessionMessage but the shape TARGETS agt:Message (asymmetry).
        assert!(
            body.contains("sh:targetClass <http://mnemosyne.dev/agent#Message>"),
            "SessionMessage shape targets agt:Message (kind≠rdf_type): {body}"
        );
        // The required generic-record field `text` derives agt:text with a minCount.
        assert!(body.contains("sh:path <http://mnemosyne.dev/agent#text>"));
        assert!(body.contains("sh:minCount 1"));
        // The SessionState head shape targets agt:SessionState (key == rdf_type here).
        assert!(body.contains("sh:targetClass <http://mnemosyne.dev/agent#SessionState>"));
    }

    #[test]
    fn openapi_face_renders_a_valid_openapi_3x_document() {
        // The EA-3 Seq 9 OpenAPI face renders the sophia-api vocab as a valid
        // OpenAPI 3.x document (info from the contract metadata, the operations
        // surface deferred to the live Seq 10 caller), served as
        // application/openapi+json.
        let contract = crate::emporium::vocabs::find_contract("sophia-api", "latest").unwrap();
        let (body, media) = render_body(&contract, VocabFormat::OpenApiJson);
        assert_eq!(media, "application/openapi+json");
        // The body MUST parse as JSON and be a valid OpenAPI 3.x root.
        let spec: serde_json::Value =
            serde_json::from_str(&body).expect("openapi face body must be valid JSON");
        assert!(
            spec["openapi"].as_str().unwrap().starts_with("3."),
            "openapi version field present and 3.x: {body}"
        );
        assert_eq!(spec["info"]["title"], "Sophia API Publication Vocabulary");
        assert_eq!(spec["info"]["version"], "1.0.0");
        assert!(spec["paths"].is_object(), "paths object present");
        assert!(
            spec["components"]["schemas"].is_object(),
            "components.schemas present"
        );
    }

    #[test]
    fn workflow_openapi_face_is_addressable_canonically() {
        let contract = crate::emporium::vocabs::find_contract("workflow", "latest").unwrap();
        let (body, media) = render_body(&contract, VocabFormat::OpenApiJson);
        assert_eq!(media, "application/openapi+json");
        let spec: serde_json::Value =
            serde_json::from_str(&body).expect("workflow openapi face body must be valid JSON");
        assert!(spec["openapi"].as_str().unwrap().starts_with("3."));
        assert_eq!(spec["info"]["title"], "Mnemosyne Workflow Vocabulary");
        assert!(spec["paths"].is_object(), "paths object present");
    }

    #[test]
    fn openapi_yaml_face_serves_the_same_spec_under_the_yaml_media_type() {
        let contract = crate::emporium::vocabs::find_contract("sophia-api", "latest").unwrap();
        let (body, media) = render_body(&contract, VocabFormat::OpenApiYaml);
        assert_eq!(media, "application/openapi+yaml");
        // A JSON document is valid YAML; it still parses as JSON here.
        let spec: serde_json::Value = serde_json::from_str(&body).expect("valid JSON (and YAML)");
        assert!(spec["openapi"].as_str().unwrap().starts_with("3."));
    }

    #[test]
    fn vocab_response_serves_openapi_via_content_negotiation() {
        // End-to-end through the shared serving logic: ?format=openapi → 200 with
        // the OpenAPI media type + ETag/Cache-Control, body is a valid 3.x doc.
        let response = vocab_response("sophia-api", "latest", Some("openapi"), &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/openapi+json"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            CACHE_CONTROL
        );
        // Same route honored via the Accept header.
        let accept = headers_with(header::ACCEPT, "application/openapi+json");
        let via_accept = vocab_response("sophia-api", "latest", None, &accept);
        assert_eq!(via_accept.status(), StatusCode::OK);
        assert_eq!(
            via_accept
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/openapi+json"
        );
    }

    #[test]
    fn populated_workflow_openapi_response_uses_workflow_contract_metadata() {
        let store = oxigraph::store::Store::new().unwrap();
        let response =
            populated_workflow_openapi_response(&store, "lab", Some("openapi"), &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/openapi+json"
        );
        let body_bytes =
            crate::app_runtime::async_runtime::block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
                .expect("collect served body");
        let spec: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("served workflow OpenAPI JSON");
        assert_eq!(spec["info"]["title"], "Mnemosyne Workflow Vocabulary");
        assert!(spec["paths"].as_object().unwrap().is_empty());
    }

    #[test]
    fn vocab_response_404_for_unknown_name() {
        let response = vocab_response("nope", "latest", None, &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn vocab_response_304_when_etag_matches() {
        let sha = crate::emporium::vocabs::WORKFLOW_GOLDEN_SHA;
        let headers = headers_with(header::IF_NONE_MATCH, &format!("\"{sha}\""));
        let response = vocab_response("workflow", "latest", None, &headers);
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let etag = response.headers().get(header::ETAG).unwrap();
        assert_eq!(etag.to_str().unwrap(), format!("\"{sha}\""));
    }

    #[test]
    fn vocab_response_200_attaches_etag_and_cache_control() {
        let response = vocab_response("workflow", "latest", Some("json"), &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            CACHE_CONTROL
        );
        let sha = crate::emporium::vocabs::WORKFLOW_GOLDEN_SHA;
        assert_eq!(
            response
                .headers()
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap(),
            format!("\"{sha}\"")
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    //  EA-3 WORKED-ONCE PROOF (CA-1 / DoD item 1): publish → project → SERVE →
    //  re-parse → SHACL-conformant, for the §6 four-instance acceptance set.
    //
    //  NO MOCK. The SHACL shapes are SERVED through the REAL conneg route
    //  (`vocab_response(..,"turtle",..)` = the exact `?format=turtle` curl), the
    //  served turtle is RE-PARSED into a real oxigraph Store, and each agent-
    //  subject instance is RE-VALIDATED against THAT served shapes graph through
    //  the real two-face validator (rudof structural + the oxigraph sh:select
    //  evaluator) under its real per-observer membrane. The EA-2b conformance-
    //  oracle pattern, extended to agt: and driven end-to-end through the HTTP
    //  serve face.
    // ════════════════════════════════════════════════════════════════════════

    use crate::emporium::shacl_validator::validate_against_shapes;
    use crate::emporium::terms::{Term, Triple};
    use oxigraph::model::{Literal, NamedNode};

    const AGT: &str = "http://mnemosyne.dev/agent#";
    const MEM: &str = "http://mnemosyne.dev/memory#";
    const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

    fn uri(s: &str) -> Term {
        Term::Uri(NamedNode::new(s).expect("valid IRI"))
    }
    fn lit(s: &str) -> Term {
        Term::Lit(Literal::new_simple_literal(s))
    }

    /// CURL the SHACL shapes through the REAL conneg serve route and return the
    /// served turtle body verbatim. This is the published face an agent GETs —
    /// `GET /emporium/vocab/sophia-agent-core/latest?format=turtle`.
    fn curl_served_shacl(name: &str) -> String {
        let response = vocab_response(name, "latest", Some("turtle"), &HeaderMap::new());
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "serve {name} turtle = 200"
        );
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/turtle",
            "served under text/turtle"
        );
        let bytes =
            crate::app_runtime::async_runtime::block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
                .expect("collect served shapes body");
        String::from_utf8(bytes.to_vec()).expect("served turtle is utf-8")
    }

    /// Re-validate one agent-subject instance against the SERVED shapes turtle in
    /// its real membrane graph, and assert SHACL-CONFORMANT: no `sh:Violation`.
    /// Advisory `sh:Warning` solutions (I1-Valuation, I3d) are NOT violations —
    /// they are observable, expected design-gap markers, so they are tolerated
    /// (and reported) but never fail conformance.
    fn assert_conformant(label: &str, served_shapes: &str, instance: &[Triple], membrane: &str) {
        match validate_against_shapes(served_shapes, instance, membrane, "sophia-agent-core") {
            Ok(()) => {}
            Err(violations) => {
                let hard: Vec<_> = violations
                    .iter()
                    .filter(|v| v.severity == "Violation")
                    .collect();
                assert!(
                    hard.is_empty(),
                    "[{label}] served-turtle re-validation found sh:Violation(s) — instance is NOT SHACL-conformant: {hard:#?}"
                );
            }
        }
    }

    /// THE EA-3 WORKED-ONCE PROOF. One test, four instances, all driven through
    /// the SAME served-turtle shapes (curled once via the real route).
    #[test]
    fn ea3_curl_back_four_instances_revalidate_through_the_served_face() {
        // ── SERVE + RE-PARSE: curl the §3 SHACL through the real conneg route,
        //    then prove the served turtle is REAL, loadable SHACL (parses into a
        //    fresh oxigraph Store) and carries the §3 sh:select invariants.
        let served = curl_served_shacl("sophia-agent-core");
        assert!(
            served.contains("a sh:NodeShape"),
            "served body is real SHACL"
        );
        assert!(
            served.contains("sh:sparql"),
            "served body carries the §3 sh:select invariants"
        );
        {
            let store = oxigraph::store::Store::new().unwrap();
            store
                .load_from_slice(oxigraph::io::RdfFormat::Turtle, served.as_bytes())
                .expect("the SERVED turtle re-parses into a real oxigraph Store");
            let n = crate::emporium::shacl_sparql::extract_sparql_constraints(&served)
                .expect("the served shapes graph yields its §3 sh:select constraints");
            assert!(
                n.len() >= 7,
                "the served face carries the §3 membrane/cardinality/identity invariants: {}",
                n.len()
            );
        }

        // Membrane stems. §4 identity is LANDED, so the agent IRI byte-matches
        // L0's observer_iri: urn:sophia:agent:{agentId}, membrane segment = {agentId}.
        let id_a = "agent-1a2b3c4d5e6f7a8b";
        let agent_a = format!("urn:sophia:agent:{id_a}");
        let membrane_a = format!("urn:mnemosyne:local:graph:lab:projection:memory:agent:{id_a}");

        // ── (a) A SINGLE WORKED AGENT — one Monovocal membrane, one witnessed
        //    record (the leaf elided per the degeneracy theorem). Exercises I1
        //    (witnessed), I3 (Monovocal ⟺ exactly one voice), IdentityUnification.
        {
            let voice = format!("{agent_a}#voice:solo");
            let rec = format!("{membrane_a}#record:abc");
            let instance: Vec<Triple> = vec![
                // the agent subject (unified IRI; Monovocal; one voice)
                (
                    agent_a.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{AGT}Agent")),
                ),
                (agent_a.clone(), format!("{AGT}agentId"), lit(id_a)),
                (
                    agent_a.clone(),
                    format!("{AGT}voicing"),
                    uri(&format!("{AGT}Monovocal")),
                ),
                (agent_a.clone(), format!("{AGT}hasVoice"), uri(&voice)),
                // its single voice (borrows the membrane, owns none)
                (voice.clone(), RDF_TYPE.into(), uri(&format!("{AGT}Voice"))),
                (voice.clone(), format!("{AGT}voiceOf"), uri(&agent_a)),
                (voice.clone(), format!("{AGT}voiceId"), lit("solo")),
                // one witnessed membrane record (leaf elided — Monovocal)
                (
                    rec.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{MEM}MemoryRecord")),
                ),
                (rec.clone(), format!("{MEM}observedBy"), uri(&agent_a)),
            ];
            assert_conformant("a: single worked agent", &served, &instance, &membrane_a);
        }

        // ── (b) A PARLIAMENT — ≥2 voices, leaf-distinguished (mem:agentId) in ONE
        //    membrane. Exercises I2 (voice containment), I2b (leaf needs witness),
        //    I3 (Parliament ⟺ ≥2 voices), I4b (voiced record's observedBy = agent).
        {
            let v_arch = format!("{agent_a}#voice:archivist");
            let v_crit = format!("{agent_a}#voice:critic");
            let rec1 = format!("{membrane_a}#record:p1");
            let rec2 = format!("{membrane_a}#record:p2");
            let instance: Vec<Triple> = vec![
                (
                    agent_a.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{AGT}Agent")),
                ),
                (agent_a.clone(), format!("{AGT}agentId"), lit(id_a)),
                (
                    agent_a.clone(),
                    format!("{AGT}voicing"),
                    uri(&format!("{AGT}Parliament")),
                ),
                (agent_a.clone(), format!("{AGT}hasVoice"), uri(&v_arch)),
                (agent_a.clone(), format!("{AGT}hasVoice"), uri(&v_crit)),
                (v_arch.clone(), RDF_TYPE.into(), uri(&format!("{AGT}Voice"))),
                (v_arch.clone(), format!("{AGT}voiceOf"), uri(&agent_a)),
                (v_arch.clone(), format!("{AGT}voiceId"), lit("archivist")),
                (v_crit.clone(), RDF_TYPE.into(), uri(&format!("{AGT}Voice"))),
                (v_crit.clone(), format!("{AGT}voiceOf"), uri(&agent_a)),
                (v_crit.clone(), format!("{AGT}voiceId"), lit("critic")),
                // two leaf-distinguished records, both witnessed by the agent
                (
                    rec1.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{MEM}MemoryRecord")),
                ),
                (rec1.clone(), format!("{MEM}observedBy"), uri(&agent_a)),
                (rec1.clone(), format!("{MEM}agentId"), lit("archivist")),
                (
                    rec2.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{MEM}MemoryRecord")),
                ),
                (rec2.clone(), format!("{MEM}observedBy"), uri(&agent_a)),
                (rec2.clone(), format!("{MEM}agentId"), lit("critic")),
            ];
            assert_conformant(
                "b: parliament (≥2 leaf-distinguished voices)",
                &served,
                &instance,
                &membrane_a,
            );
        }

        // ── (c) A CROSS-AGENT PAIR — two distinct agent membranes, no shared IRIs.
        //    Each is its own Monovocal witness; cross-agent isolation = distinct
        //    graphs. We validate the SECOND agent in ITS membrane to prove the firm
        //    boundary holds for a second, disjoint perspective.
        {
            let id_b = "agent-9f8e7d6c5b4a3210";
            let agent_b = format!("urn:sophia:agent:{id_b}");
            let membrane_b =
                format!("urn:mnemosyne:local:graph:lab:projection:memory:agent:{id_b}");
            let voice_b = format!("{agent_b}#voice:solo");
            let rec_b = format!("{membrane_b}#record:only");
            let instance: Vec<Triple> = vec![
                (
                    agent_b.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{AGT}Agent")),
                ),
                (agent_b.clone(), format!("{AGT}agentId"), lit(id_b)),
                (
                    agent_b.clone(),
                    format!("{AGT}voicing"),
                    uri(&format!("{AGT}Monovocal")),
                ),
                (agent_b.clone(), format!("{AGT}hasVoice"), uri(&voice_b)),
                (
                    voice_b.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{AGT}Voice")),
                ),
                (voice_b.clone(), format!("{AGT}voiceOf"), uri(&agent_b)),
                (voice_b.clone(), format!("{AGT}voiceId"), lit("solo")),
                (
                    rec_b.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{MEM}MemoryRecord")),
                ),
                (rec_b.clone(), format!("{MEM}observedBy"), uri(&agent_b)),
            ];
            // The agent IRIs / membranes share NO bytes with agent A's.
            assert!(
                !agent_b.contains(id_a) && !membrane_b.contains(id_a),
                "no shared IRIs"
            );
            assert_conformant(
                "c: cross-agent pair (second disjoint membrane)",
                &served,
                &instance,
                &membrane_b,
            );
        }

        // ── (d) A COMMONS RECORD — empty observer, un-segmented graph, NO
        //    mem:observedBy. MUST pass I1 (the must-fix #1 regression guard): the
        //    firm-boundary invariant is scoped to per-observer membrane graphs, so
        //    a witnessless record in the COMMONS is legal, not a violation.
        {
            let commons = "urn:mnemosyne:local:graph:lab:projection:memory";
            let rec = format!("{commons}#record:xyz");
            let instance: Vec<Triple> = vec![
                (
                    rec.clone(),
                    RDF_TYPE.into(),
                    uri(&format!("{MEM}MemoryRecord")),
                ),
                // deliberately NO mem:observedBy — the commons is un-witnessed
            ];
            assert_conformant(
                "d: commons record (must-fix #1 regression guard)",
                &served,
                &instance,
                commons,
            );
        }
    }

    /// TEETH for the curl-back proof: the SAME served-face validator MUST reject a
    /// malformed instance, so the four-instance pass above is not a pass-everything
    /// tautology. A membrane record with NO witness trips I1 as a hard sh:Violation.
    #[test]
    fn ea3_curl_back_served_face_catches_a_witnessless_membrane_record() {
        let served = curl_served_shacl("sophia-agent-core");
        let id = "agent-deadbeefcafef00d";
        let membrane = format!("urn:mnemosyne:local:graph:lab:projection:memory:agent:{id}");
        let rec = format!("{membrane}#record:naked");
        let witnessless: Vec<Triple> = vec![
            (
                rec.clone(),
                RDF_TYPE.into(),
                uri(&format!("{MEM}MemoryRecord")),
            ),
            // NO mem:observedBy inside a per-observer membrane → I1 Violation
        ];
        let violations =
            validate_against_shapes(&served, &witnessless, &membrane, "sophia-agent-core")
                .expect_err("a witnessless membrane record must NOT be conformant");
        assert!(
            violations.iter().any(|v| v.severity == "Violation"
                && v.shape.as_deref()
                    == Some("http://mnemosyne.dev/agent#I1_MembraneWitnessShape")),
            "I1 fires as a hard sh:Violation through the SERVED face: {violations:#?}"
        );
    }

    // ── T2 item 11: the query-face catalog's Markdown rendering ──

    #[test]
    fn query_catalog_markdown_shows_all_four_query_names_with_fenced_sparql() {
        let contract = crate::emporium::contract::memory_core_vocabulary();
        let catalog =
            crate::emporium::query_emit::emit_query_catalog(contract, "MemoryRecord").unwrap();
        let markdown = render_query_catalog_markdown(&catalog);
        assert!(markdown.starts_with("# sophia-memory-core / MemoryRecord — query catalog"));
        for name in ["byId", "countBy", "currentHeads", "lineageOf"] {
            assert!(
                markdown.contains(&format!("## {name}")),
                "missing section for {name}: {markdown}"
            );
        }
        assert!(markdown.contains("```sparql"));
        assert!(
            !markdown.contains("_unsupported"),
            "MemoryRecord supports all four plans"
        );
    }

    #[test]
    fn query_catalog_markdown_explains_unsupported_plans_without_a_dead_template() {
        let contract = get_vocabulary("emporium-bookmark").expect("emporium-bookmark registered");
        let catalog =
            crate::emporium::query_emit::emit_query_catalog(contract, "Bookmark").unwrap();
        let markdown = render_query_catalog_markdown(&catalog);
        assert!(markdown.contains("## currentHeads"));
        assert!(
            markdown.contains("_unsupported for this class:"),
            "Bookmark has no lineage predicate: {markdown}"
        );
    }

    #[test]
    fn query_catalog_route_serves_json_by_default_and_404s_an_unknown_vocab() {
        let ok = crate::app_runtime::async_runtime::block_on(get_vocab_class_query(
            Path(("sophia-memory-core".to_string(), "MemoryRecord".to_string())),
            Query(FormatQuery { format: None }),
            HeaderMap::new(),
        ));
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(
            ok.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );

        let missing = crate::app_runtime::async_runtime::block_on(get_vocab_class_query(
            Path(("no-such-vocab".to_string(), "Whatever".to_string())),
            Query(FormatQuery { format: None }),
            HeaderMap::new(),
        ));
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
}
