use crate::rdf::parse_rdf_format;
use crate::rdf_authority::is_reserved_rdf_graph_iri;
use oxigraph::{
    io::{RdfFormat, RdfParser},
    model::{GraphName, GraphNameRef, NamedNodeRef},
    sparql::{CancellationToken, QueryResults, SparqlEvaluator},
    store::Store,
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MutationResult {
    pub(super) ok: bool,
    pub(super) quad_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SparqlQueryResult {
    pub(super) result_type: String,
    pub(super) variables: Vec<String>,
    pub(super) rows: Vec<BTreeMap<String, String>>,
    pub(super) boolean: Option<bool>,
    pub(super) graph: Option<String>,
    pub(super) quad_count: usize,
    /// ADDITIVE, best-effort nudges about the result shape. Absent (field
    /// omitted from the JSON entirely) when there is nothing to say, so
    /// existing consumers that don't know this key exists are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) warnings: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RdfDumpResult {
    pub(super) format: String,
    pub(super) media_type: String,
    pub(super) data: String,
    pub(super) quad_count: usize,
}

/// Hard ceiling on materialized SPARQL results, overridable via
/// `GARDEN_SPARQL_MAX_ROWS`. The result collector below materializes EVERY
/// solution row as a `BTreeMap<String, String>` (~700B/row with term
/// rendering) before anything is serialized — an unbounded `SELECT` over a
/// multi-million-quad store is therefore a cell-killer: 20M rows ≈ 14GB of
/// anon memory in one tight collect loop, allocated at GBs/second with no
/// log output (a fresh 14Gi canary cell was OOMKilled this way, 2026-07-22).
/// Truncation is announced in `warnings` AND the log; a truncated answer
/// with a warning strictly beats an OOMKilled cell.
///
/// This ceiling is applied ONLY at the EXTERNAL boundary
/// (`execute_sparql_query_capped`), never in the shared `execute_sparql_query`
/// primitive: internal consumers that collect/paginate results (Emporium
/// object/class queries, the materializers, seed reconcile) must see every
/// row — a silent cap there drops real members with no signal.
const SPARQL_MAX_ROWS_ENV: &str = "GARDEN_SPARQL_MAX_ROWS";
const SPARQL_MAX_ROWS_DEFAULT: usize = 50_000;

#[cfg(test)]
thread_local! {
    /// Thread-local (NOT env) so a tiny test cap can never leak into the
    /// many other tests that run SPARQL reads concurrently in this binary.
    static SPARQL_MAX_ROWS_OVERRIDE_FOR_TEST: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn set_sparql_max_rows_for_test(value: Option<usize>) {
    SPARQL_MAX_ROWS_OVERRIDE_FOR_TEST.with(|cell| cell.set(value));
}

pub(super) fn sparql_server_max_rows() -> usize {
    #[cfg(test)]
    if let Some(value) = SPARQL_MAX_ROWS_OVERRIDE_FOR_TEST.with(|cell| cell.get()) {
        return value;
    }
    std::env::var(SPARQL_MAX_ROWS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(SPARQL_MAX_ROWS_DEFAULT)
}

/// UNCAPPED SPARQL execution — the shared internal primitive. Every internal
/// consumer (Emporium object/class queries, the materializers, seed reconcile)
/// uses this and MUST receive every row: a silent row cap here truncates a
/// collected/paginated result set with NO signal (e.g. a class with >cap
/// members silently loses members behind a discarded warning). The row/triple
/// ceiling belongs ONLY at the external boundary — see
/// [`execute_sparql_query_capped`].
pub(super) fn execute_sparql_query(
    store: &Store,
    query: &str,
) -> Result<SparqlQueryResult, String> {
    execute_sparql_query_inner(store, query, None, None)
}

/// EXTERNAL-boundary SPARQL execution: applies the materialized-row ceiling
/// (`GARDEN_SPARQL_MAX_ROWS`, default 50k) and announces truncation in
/// `warnings` + the log. Use ONLY where a human or agent submits an arbitrary
/// query and can act on the warning — the MCP `sparql_query` tool, the Tauri
/// IPC command, and the loopback HTTP route, all routed through
/// `run_sparql_query_service`. A truncated answer with a warning strictly
/// beats the OOMKilled cell an unbounded external SELECT would cause.
pub(super) fn execute_sparql_query_capped(
    store: &Store,
    query: &str,
) -> Result<SparqlQueryResult, String> {
    execute_sparql_query_inner(store, query, Some(sparql_server_max_rows()), None)
}

/// Externally controlled query execution.
///
/// Unlike output-only truncation, this receives Oxigraph's cooperative
/// cancellation token. The admission layer fires it on request deadline or
/// client disconnect; the collector also fires it as soon as the row/triple
/// ceiling is reached so no further algebra work remains admissible.
pub(super) fn execute_sparql_query_capped_with_control(
    store: &Store,
    query: &str,
    max_rows: usize,
    cancellation: CancellationToken,
) -> Result<SparqlQueryResult, String> {
    execute_sparql_query_inner(store, query, Some(max_rows.max(1)), Some(cancellation))
}

/// Shared implementation. `max_rows` is `Some(cap)` only at the external
/// boundary; internal callers pass `None` for an uncapped collect.
fn execute_sparql_query_inner(
    store: &Store,
    query: &str,
    max_rows: Option<usize>,
    cancellation: Option<CancellationToken>,
) -> Result<SparqlQueryResult, String> {
    let quad_count = store
        .len()
        .map_err(|error| format!("count quads: {error}"))?;
    let mut evaluator = SparqlEvaluator::new();
    if let Some(cancellation) = cancellation.as_ref() {
        evaluator = evaluator.with_cancellation_token(cancellation.clone());
    }
    let results = evaluator
        .parse_query(query)
        .map_err(|error| format!("parse SPARQL query: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute SPARQL query: {error}"))?;

    match results {
        QueryResults::Solutions(mut solutions) => {
            let variables = solutions
                .variables()
                .iter()
                .map(|variable| variable.as_str().to_string())
                .collect::<Vec<_>>();
            let mut rows = Vec::new();
            let mut truncated = false;
            while let Some(solution) = solutions.next() {
                let solution =
                    solution.map_err(|error| format!("read SPARQL solution: {error}"))?;
                if max_rows.is_some_and(|cap| rows.len() >= cap) {
                    if let Some(cancellation) = cancellation.as_ref() {
                        cancellation.cancel();
                    }
                    truncated = true;
                    break;
                }
                let mut row = BTreeMap::new();
                for variable in &variables {
                    if let Some(term) = solution.get(variable.as_str()) {
                        row.insert(variable.clone(), term.to_string());
                    }
                }
                rows.push(row);
            }
            let mut warnings =
                silent_zero_warnings(query, rows.is_empty(), quad_count).unwrap_or_default();
            if truncated {
                let cap = max_rows.unwrap_or_default();
                log::warn!(
                    "sparql query result truncated at {cap} rows \
                     (store holds {quad_count} quads); raise {SPARQL_MAX_ROWS_ENV} or add \
                     LIMIT/OFFSET paging"
                );
                warnings.push(format!(
                    "result truncated at {cap} rows ({SPARQL_MAX_ROWS_ENV}); \
                     add LIMIT/OFFSET paging to see the rest"
                ));
            }
            let warnings = if warnings.is_empty() {
                None
            } else {
                Some(warnings)
            };
            Ok(SparqlQueryResult {
                result_type: "solutions".to_string(),
                variables,
                rows,
                boolean: None,
                graph: None,
                quad_count,
                warnings,
            })
        }
        QueryResults::Boolean(value) => Ok(SparqlQueryResult {
            result_type: "boolean".to_string(),
            variables: Vec::new(),
            rows: Vec::new(),
            boolean: Some(value),
            graph: None,
            quad_count,
            warnings: None,
        }),
        QueryResults::Graph(triples) => {
            let mut graph = String::new();
            let mut triple_count = 0usize;
            let mut truncated = false;
            for triple in triples {
                let triple =
                    triple.map_err(|error| format!("read SPARQL graph result: {error}"))?;
                if max_rows.is_some_and(|cap| triple_count >= cap) {
                    if let Some(cancellation) = cancellation.as_ref() {
                        cancellation.cancel();
                    }
                    truncated = true;
                    break;
                }
                graph.push_str(&format!("{triple} .\n"));
                triple_count += 1;
            }
            let warnings = truncated.then(|| {
                let cap = max_rows.unwrap_or_default();
                log::warn!(
                    "sparql CONSTRUCT/DESCRIBE result truncated at {cap} triples \
                     (store holds {quad_count} quads); raise {SPARQL_MAX_ROWS_ENV} or narrow \
                     the pattern"
                );
                vec![format!(
                    "graph result truncated at {cap} triples ({SPARQL_MAX_ROWS_ENV}); \
                     narrow the pattern to see the rest"
                )]
            });
            Ok(SparqlQueryResult {
                result_type: "graph".to_string(),
                variables: Vec::new(),
                rows: Vec::new(),
                boolean: None,
                graph: Some(graph),
                quad_count,
                warnings,
            })
        }
    }
}

/// The "silent zero": gardend stores are named-graph-partitioned (every
/// `user:rdf` load lands under a named graph, not the store default graph),
/// so a bare `SELECT` with no `GRAPH` clause silently matches zero rows even
/// though the store holds plenty of quads. That's indistinguishable, from the
/// JSON alone, from "the store is actually empty" or "your pattern doesn't
/// match" — so flag it when it's plausible.
///
/// Fires only for `SELECT`-shaped (`Solutions`) results: `rows.is_empty()`
/// AND `quad_count > 0` AND the query text has no `GRAPH` token at all. `ASK`
/// and `CONSTRUCT` results don't carry this signal (their `rows` is always
/// empty regardless of named-graph scoping), so they never emit it.
fn silent_zero_warnings(query: &str, rows_empty: bool, quad_count: usize) -> Option<Vec<String>> {
    if rows_empty && quad_count > 0 && !query_mentions_graph_keyword(query) {
        Some(vec![format!(
            "query has no GRAPH clause but this store is named-graph-partitioned \
             ({quad_count} quads live in named graphs); wrap your pattern in \
             GRAPH ?g {{ }} or target an explicit graph IRI"
        )])
    } else {
        None
    }
}

/// Case-insensitive keyword heuristic for "does this query text reference a
/// `GRAPH` clause" — tokenization-aware: string literals (single/double/
/// triple-quoted, with backslash escapes honored), IRIREFs (`<...>`), and
/// `#`-to-end-of-line comments are stripped ([`strip_sparql_literals_and_comments`])
/// before scanning, so a literal, IRI, or comment merely CONTAINING the
/// substring "graph" no longer produces a false NEGATIVE (silently
/// suppressing the warning on a query that has no real `GRAPH { }` block —
/// e.g. `SELECT ?o WHERE { ?s <urn:p> "this graph rocks" }`, or one commented
/// `# GRAPH scoping TODO`). Still deliberately NOT a real SPARQL algebra walk
/// — this only gates a best-effort warning, not query correctness. Remaining
/// documented edges:
///   - the bare substring "graph" surviving the strip inside a variable name
///     (`?graph`) or a prefixed name's local part (`ex:graph`) with no actual
///     `GRAPH { }` block still counts as a false negative (word-boundary
///     matching is future work, not tackled here);
///   - a query-level `FROM <iri>` dataset clause re-scopes the default graph
///     without ever spelling the `GRAPH` keyword, so the reverse can happen:
///     the warning fires even though the query already explicitly targeted a
///     graph via `FROM`.
/// Both directions bias toward an occasional wrong (extra or missing) nudge
/// rather than the cost of a full parse for what is documented as advisory.
fn query_mentions_graph_keyword(query: &str) -> bool {
    strip_sparql_literals_and_comments(query)
        .to_ascii_uppercase()
        .contains("GRAPH")
}

/// Strip SPARQL string literals (`'...'`, `"..."`, `'''...'''`, `"""..."""`,
/// with `\`-escapes honored so an escaped quote never ends the literal
/// early), IRIREFs (`<...>`), and `#`-to-end-of-line comments from `query`,
/// replacing each with a single space (so tokens on either side never fuse
/// into a new word). A lightweight tokenizer, not a full SPARQL parser —
/// good enough to keep [`query_mentions_graph_keyword`] from being fooled by
/// the word "graph" living inside quoted/commented text rather than an
/// actual `GRAPH` keyword.
fn strip_sparql_literals_and_comments(query: &str) -> String {
    let chars: Vec<char> = query.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '#' => {
                // Line comment: skip to (not including) the newline, so the
                // newline itself still separates tokens on either side.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '<' => {
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // consume the closing '>'
                }
            }
            quote @ ('"' | '\'') => {
                let triple = i + 2 < chars.len() && chars[i + 1] == quote && chars[i + 2] == quote;
                let delim_len = if triple { 3 } else { 1 };
                out.push(' ');
                i += delim_len;
                loop {
                    if i >= chars.len() {
                        break;
                    }
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 2;
                        continue;
                    }
                    if triple {
                        if i + 2 < chars.len()
                            && chars[i] == quote
                            && chars[i + 1] == quote
                            && chars[i + 2] == quote
                        {
                            i += 3;
                            break;
                        }
                    } else if chars[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

pub(super) fn execute_sparql_update(store: &Store, update: &str) -> Result<MutationResult, String> {
    SparqlEvaluator::new()
        .parse_update(update)
        .map_err(|error| format!("parse SPARQL update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute SPARQL update: {error}"))?;
    crate::cell_durability::mark_rdf_store_written(store);
    mutation_result(store)
}

pub(super) fn load_rdf_into_store(
    store: &Store,
    data: &str,
    format: &str,
    base_iri: Option<&str>,
    target_graph_iri: Option<&str>,
) -> Result<MutationResult, String> {
    let format = parse_rdf_format(format)?;
    let mut parser = RdfParser::from_format(format);
    if let Some(base_iri) = base_iri.filter(|value| !value.trim().is_empty()) {
        parser = parser
            .with_base_iri(base_iri)
            .map_err(|error| format!("invalid base IRI: {error}"))?;
    }
    if let Some(graph_iri) = target_graph_iri.filter(|value| !value.trim().is_empty()) {
        parser = parser.without_named_graphs().with_default_graph(
            NamedNodeRef::new(graph_iri)
                .map_err(|error| format!("invalid target graph IRI: {error}"))?,
        );
    }
    store
        .load_from_slice(parser, data.as_bytes())
        .map_err(|error| format!("load RDF: {error}"))?;
    crate::cell_durability::mark_rdf_store_written(store);
    mutation_result(store)
}

/// Distinguishes WHY the load_rdf_dataset pre-check refused a payload, so the
/// service layer can map each cause to the right `AppError` kind (review r1,
/// finding "malformed payload failures remain genuinely distinct"): a
/// `Reserved` target is a client policy violation — `AppError::validation` →
/// HTTP 400, the SAME kind `validate_sparql_update_authority` and
/// `user_rdf_target_graph_iri` already use for a reserved-graph refusal — while
/// `ParseFailure` (bad format name, invalid base IRI, or a syntax error the
/// parser itself rejects) is a malformed-payload failure, kept as
/// `AppError::rdf` → HTTP 500, consistent with `load_rdf_into_store`'s own
/// parse-failure mapping for the exact same class of error on the `load_rdf`
/// path. Both variants still REFUSE the import — this only changes which
/// `AppError` kind carries the refusal, never whether one happens.
pub(super) enum RdfDatasetTargetRefusal {
    Reserved(String),
    ParseFailure(String),
}

impl RdfDatasetTargetRefusal {
    /// Test-only: production code matches on the variant directly
    /// (`rdf_service.rs::load_rdf_dataset_service`); this accessor exists so
    /// unit tests can assert the classification without a full `match`.
    #[cfg(test)]
    pub(super) fn is_reserved(&self) -> bool {
        matches!(self, Self::Reserved(_))
    }

    pub(super) fn into_message(self) -> String {
        match self {
            Self::Reserved(message) | Self::ParseFailure(message) => message,
        }
    }
}

/// A9 (§A.6/A.9 of `observatory-analysis-cell-spec-20260715.md`): `load_rdf`
/// is checked upstream because its ONE target graph IRI is an explicit,
/// already-known argument (`user_rdf_target_graph_iri`, `rdf_service.rs`),
/// but a dataset-format payload (TriG/N-Quads/JSON-LD) carries an arbitrary
/// NUMBER of named-graph IRIs INLINE in `data` itself — there is nothing to
/// check until the payload is parsed. So this scan runs a full, SEPARATE
/// parse of `data` with the exact same `format`/`base_iri` the real load
/// will use, and refuses the whole import the moment any quad names a
/// reserved graph — before a single quad ever reaches `store.load_from_slice`.
/// Reuses `is_reserved_rdf_graph_iri` verbatim (the same predicate
/// `validate_sparql_update_authority`/`user_rdf_target_graph_iri` already
/// gate on) so the reserved-ness definition can never drift between the
/// SPARQL-update path, the `load_rdf` path, and this one.
///
/// Only `GraphName::NamedNode` graph names are checked against the reserved
/// prefix. `GraphName::DefaultGraph` (every quad from a non-dataset format —
/// Turtle/N-Triples/RDF-XML, whose `RdfFormat::supports_datasets()` is false —
/// plus every N3 quad OUTSIDE a `{ }` formula) has no IRI and can never
/// collide with a reserved IRI by construction. A blank-node graph name
/// (`_:g { … }` in TriG, `… _:g .` in N-Quads, or an N3 `{ … }` formula —
/// oxigraph represents ALL THREE as `GraphName::BlankNode`, verified against
/// this exact oxigraph/oxrdfio version, not assumed) is likewise a fresh,
/// process-scoped identifier of a DIFFERENT RDF term kind than `NamedNode` —
/// it can never literally BE a reserved IRI, by the type system, not by
/// coincidence of value — so it is intentionally not compared against
/// `is_reserved_rdf_graph_iri` at all (see the blank-node acceptance tests
/// below, and `observatory_authority.rs`'s Direction-6 blank-node-graph
/// siblings for the real-store proof). FAILS CLOSED on anything else: a parse
/// error surfaced during this scan is returned verbatim (refuse rather than
/// silently admit an ambiguous or un-parseable graph target) — the real
/// load's own parse of the same bytes would hit the identical error
/// regardless, so this changes nothing for a legitimate, well-formed payload.
fn reject_reserved_named_graphs_in_dataset(
    graph_id: &str,
    data: &str,
    format: RdfFormat,
    base_iri: Option<&str>,
) -> Result<(), RdfDatasetTargetRefusal> {
    let mut parser = RdfParser::from_format(format);
    if let Some(base_iri) = base_iri.filter(|value| !value.trim().is_empty()) {
        parser = parser.with_base_iri(base_iri).map_err(|error| {
            RdfDatasetTargetRefusal::ParseFailure(format!("invalid base IRI: {error}"))
        })?;
    }
    for quad in parser.for_slice(data.as_bytes()) {
        let quad = quad.map_err(|error| {
            RdfDatasetTargetRefusal::ParseFailure(format!("load RDF dataset: {error}"))
        })?;
        if let GraphName::NamedNode(named) = &quad.graph_name {
            let graph_iri = named.as_str();
            if is_reserved_rdf_graph_iri(graph_id, graph_iri) {
                return Err(RdfDatasetTargetRefusal::Reserved(format!(
                    "RDF dataset import cannot target reserved projection graph {graph_iri}"
                )));
            }
        }
    }
    Ok(())
}

/// Same check as [`validate_rdf_dataset_graph_targets`], but keeping the
/// `Reserved`/`ParseFailure` distinction alive for a caller (the service
/// layer) that needs it to pick an `AppError` kind. `validate_rdf_dataset_graph_targets`
/// itself flattens to `String` for callers (the test harness) that only need
/// pass/fail.
pub(super) fn classify_rdf_dataset_graph_targets(
    graph_id: &str,
    data: &str,
    format_name: &str,
    base_iri: Option<&str>,
) -> Result<(), RdfDatasetTargetRefusal> {
    let format = parse_rdf_format(format_name).map_err(RdfDatasetTargetRefusal::ParseFailure)?;
    reject_reserved_named_graphs_in_dataset(graph_id, data, format, base_iri)
}

/// PUBLIC pre-check, run BEFORE `load_rdf_dataset_into_store` by
/// `load_rdf_dataset_service` — mirrors `load_rdf_service`'s explicit
/// `user_rdf_target_graph_iri(..)?` pre-check ahead of `load_rdf_into_store`
/// (`rdf_service.rs:211-213`). Kept as a SEPARATE, explicitly-called step
/// (rather than folded silently into the write) so the service layer can map
/// a REFUSED-target `Reserved` refusal to `AppError::validation` — the SAME
/// error kind (→ HTTP 400 via `loopback_http.rs:156`)
/// `validate_sparql_update_authority` and `user_rdf_target_graph_iri` already
/// use — genuinely distinct from a `ParseFailure`, which stays
/// `AppError::rdf` (→ HTTP 500); see [`classify_rdf_dataset_graph_targets`]
/// for the kind-preserving variant the service layer actually calls.
/// `load_rdf_dataset_into_store` ALSO runs this same check internally
/// (fail-closed defense-in-depth: it protects any caller — including this
/// crate's own authority-proof test harness — that reaches the store-write
/// primitive directly without going through the service pre-check first).
pub(super) fn validate_rdf_dataset_graph_targets(
    graph_id: &str,
    data: &str,
    format_name: &str,
    base_iri: Option<&str>,
) -> Result<(), String> {
    classify_rdf_dataset_graph_targets(graph_id, data, format_name, base_iri)
        .map_err(RdfDatasetTargetRefusal::into_message)
}

pub(super) fn load_rdf_dataset_into_store(
    store: &Store,
    graph_id: &str,
    data: &str,
    format: &str,
    base_iri: Option<&str>,
) -> Result<MutationResult, String> {
    let format = parse_rdf_format(format)?;
    reject_reserved_named_graphs_in_dataset(graph_id, data, format, base_iri)
        .map_err(RdfDatasetTargetRefusal::into_message)?;
    let mut parser = RdfParser::from_format(format);
    if let Some(base_iri) = base_iri.filter(|value| !value.trim().is_empty()) {
        parser = parser
            .with_base_iri(base_iri)
            .map_err(|error| format!("invalid base IRI: {error}"))?;
    }
    store
        .load_from_slice(parser, data.as_bytes())
        .map_err(|error| format!("load RDF dataset: {error}"))?;
    crate::cell_durability::mark_rdf_store_written(store);
    mutation_result(store)
}

pub(super) fn dump_rdf_from_store(
    store: &Store,
    format_name: &str,
    source_graph_iri: Option<&str>,
    graph_format_default_iri: Option<&str>,
) -> Result<RdfDumpResult, String> {
    dump_rdf_from_store_limited(store, format_name, source_graph_iri, graph_format_default_iri, None)
}

pub(super) fn dump_rdf_from_store_limited(
    store: &Store,
    format_name: &str,
    source_graph_iri: Option<&str>,
    graph_format_default_iri: Option<&str>,
    max_bytes: Option<usize>,
) -> Result<RdfDumpResult, String> {
    struct Output { bytes: Vec<u8>, limit: Option<usize> }
    impl std::io::Write for Output {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if self.limit.is_some_and(|limit| data.len() > limit.saturating_sub(self.bytes.len())) {
                return Err(std::io::Error::other("source_bundle_too_large"));
            }
            self.bytes.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let format = parse_rdf_format(format_name)?;
    let mut buffer = Output { bytes: Vec::new(), limit: max_bytes };
    let source_graph_iri = source_graph_iri
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if format.supports_datasets() && source_graph_iri.is_none() {
        buffer = store
            .dump_to_writer(format, buffer)
            .map_err(|error| format!("dump RDF dataset: {error}"))?;
    } else {
        let graph_name = graph_name_ref(
            source_graph_iri
                .or_else(|| graph_format_default_iri.map(str::trim))
                .filter(|value| !value.is_empty()),
        )?;
        store
            .dump_graph_to_writer(graph_name, format, &mut buffer)
            .map_err(|error| format!("dump RDF graph: {error}"))?;
    }

    Ok(RdfDumpResult {
        format: format.name().to_string(),
        media_type: format.media_type().to_string(),
        data: String::from_utf8(buffer.bytes)
            .map_err(|error| format!("RDF dump is not UTF-8: {error}"))?,
        quad_count: store
            .len()
            .map_err(|error| format!("count quads: {error}"))?,
    })
}

fn graph_name_ref(graph_iri: Option<&str>) -> Result<GraphNameRef<'_>, String> {
    graph_iri
        .map(|iri| {
            NamedNodeRef::new(iri)
                .map(GraphNameRef::NamedNode)
                .map_err(|error| format!("invalid RDF dump source graph IRI: {error}"))
        })
        .unwrap_or(Ok(GraphNameRef::DefaultGraph))
}

fn mutation_result(store: &Store) -> Result<MutationResult, String> {
    Ok(MutationResult {
        ok: true,
        quad_count: store
            .len()
            .map_err(|error| format!("count quads: {error}"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_pull_bounded_dump_refuses_without_truncating_or_capping_shared_dump() {
        let store = Store::new().unwrap();
        store.load_from_slice(RdfFormat::NQuads,
            b"<urn:s> <urn:p> \"meaningful retained value\" <urn:g> .\n").unwrap();
        let full = dump_rdf_from_store(&store, "nquads", None, None).unwrap();
        assert!(dump_rdf_from_store_limited(&store, "nquads", None, None, Some(full.data.len()-1)).unwrap_err().contains("source_bundle_too_large"));
        assert_eq!(dump_rdf_from_store_limited(&store, "nquads", None, None, Some(full.data.len())).unwrap().data, full.data);
        assert_eq!(dump_rdf_from_store(&store, "nquads", None, None).unwrap().data, full.data);
        assert_eq!(store.len().unwrap(), 1);
    }

    /// Regression for the cap that leaked into internal consumers: the row /
    /// triple ceiling must live at the EXTERNAL boundary
    /// (`execute_sparql_query_capped`), never in the shared primitive
    /// (`execute_sparql_query`). Internal collectors (Emporium object/class
    /// queries, the materializers) call the primitive and MUST see every row —
    /// a silent cap there paginates over a truncated vector and drops real
    /// members with no signal. With a tiny cap CONFIGURED, the primitive still
    /// returns everything uncapped while the boundary variant truncates AND
    /// announces itself in `warnings`. Real in-memory store, real query
    /// service — no mocks.
    #[test]
    fn sparql_row_cap_lives_at_the_boundary_not_the_primitive() {
        let store = Store::new().expect("store");
        for index in 0..10 {
            SparqlEvaluator::new()
                .parse_update(&format!(
                    "INSERT DATA {{ GRAPH <urn:g> {{ <urn:s{index}> <urn:p> \"v{index}\" }} }}"
                ))
                .expect("parse insert")
                .on_store(&store)
                .execute()
                .expect("insert row");
        }
        let query = "SELECT ?s ?o WHERE { GRAPH <urn:g> { ?s <urn:p> ?o } }";

        // A tiny cap is configured for BOTH calls; only the boundary honors it.
        set_sparql_max_rows_for_test(Some(5));
        let uncapped = execute_sparql_query(&store, query).expect("uncapped internal query");
        let capped = execute_sparql_query_capped(&store, query).expect("capped boundary query");
        set_sparql_max_rows_for_test(None);

        // Internal primitive: every row, no truncation warning, cap ignored.
        assert_eq!(
            uncapped.rows.len(),
            10,
            "the internal primitive must return every row regardless of the configured cap"
        );
        assert!(
            uncapped.warnings.is_none(),
            "the internal primitive must not emit a truncation warning: {:?}",
            uncapped.warnings
        );

        // External boundary: stops at the cap and announces the truncation.
        assert_eq!(
            capped.rows.len(),
            5,
            "the boundary variant stops at the cap"
        );
        let warnings = capped
            .warnings
            .expect("truncation warning present at the boundary");
        assert!(
            warnings.iter().any(|w| w.contains("truncated at 5 rows")),
            "{warnings:?}"
        );
    }

    #[test]
    fn controlled_query_uses_real_oxigraph_cancellation_and_cap_fires_it() {
        let store = Store::new().expect("store");
        for index in 0..10 {
            SparqlEvaluator::new()
                .parse_update(&format!(
                    "INSERT DATA {{ GRAPH <urn:g> {{ <urn:s{index}> <urn:p> \"v{index}\" }} }}"
                ))
                .expect("parse insert")
                .on_store(&store)
                .execute()
                .expect("insert row");
        }
        let query = "SELECT ?s ?o WHERE { GRAPH <urn:g> { ?s <urn:p> ?o } }";

        let cancelled_before_start = CancellationToken::new();
        cancelled_before_start.cancel();
        let error =
            execute_sparql_query_capped_with_control(&store, query, 5, cancelled_before_start)
                .expect_err("a pre-cancelled engine token must abort query evaluation");
        assert!(
            error.to_ascii_lowercase().contains("cancel"),
            "Oxigraph cancellation must surface distinctly: {error}"
        );

        let cap_token = CancellationToken::new();
        let capped = execute_sparql_query_capped_with_control(&store, query, 5, cap_token.clone())
            .expect("bounded query");
        assert_eq!(capped.rows.len(), 5);
        assert!(
            cap_token.is_cancelled(),
            "reaching the collector ceiling must cancel the evaluator, not merely truncate output"
        );
    }

    /// Sanity pin (not policy): confirms, against THIS exact oxigraph/oxrdfio
    /// version, that a TriG blank-node graph label, an N-Quads blank-node
    /// graph term, and an N3 `{ … }` formula are ALL represented as
    /// `GraphName::BlankNode` — the load-bearing assumption behind
    /// `reject_reserved_named_graphs_in_dataset` only ever comparing
    /// `GraphName::NamedNode` against `is_reserved_rdf_graph_iri`. If a future
    /// oxigraph upgrade ever represented one of these as a `NamedNode`
    /// instead, this test (not just the policy test below) would be the one
    /// to catch the assumption silently going stale.
    #[test]
    fn oxigraph_represents_blank_node_graph_labels_as_graphname_blanknode() {
        let trig = r#"_:g1 { <urn:s> <urn:p> <urn:o> . }"#;
        for quad in RdfParser::from_format(RdfFormat::TriG).for_slice(trig.as_bytes()) {
            let quad = quad.expect("parse trig blank-node-graph quad");
            assert!(matches!(quad.graph_name, GraphName::BlankNode(_)));
        }

        let nquads = r#"<urn:s> <urn:p> <urn:o> _:g1 ."#;
        for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(nquads.as_bytes()) {
            let quad = quad.expect("parse n-quads blank-node-graph quad");
            assert!(matches!(quad.graph_name, GraphName::BlankNode(_)));
        }

        // N3 has NO named-graph syntax (`RdfFormat::supports_datasets()` is
        // false for N3), but a `{ … }` FORMULA still produces a non-default
        // graph name — the formula's own blank node, per oxttl's N3Quad doc
        // ("graph_name is used to encode the formula ... encoded by a blank
        // node"). Distinct from every other non-dataset format, which is why
        // this is worth pinning explicitly rather than lumping N3 in with
        // Turtle/N-Triples/RDF-XML.
        let n3 = r#"<urn:s> <urn:p> { <urn:a> <urn:b> <urn:c> } ."#;
        let mut saw_formula_blank_graph = false;
        let mut saw_default_graph = false;
        for quad in RdfParser::from_format(RdfFormat::N3).for_slice(n3.as_bytes()) {
            let quad = quad.expect("parse n3 quad");
            match quad.graph_name {
                GraphName::BlankNode(_) => saw_formula_blank_graph = true,
                GraphName::DefaultGraph => saw_default_graph = true,
                GraphName::NamedNode(_) => panic!("N3 must never produce a NamedNode graph name"),
            }
        }
        assert!(
            saw_formula_blank_graph,
            "the formula's inner triple must carry a blank-node graph name"
        );
        assert!(
            saw_default_graph,
            "the outer triple (outside the formula) must stay in the default graph"
        );
    }

    /// Policy test (the review's "explicit blank-node graph-name policy
    /// test"): a blank-node graph name — the type-level opposite of a
    /// `NamedNode` — can never literally equal a reserved `:projection:*`
    /// IRI, so `load_rdf_dataset` must ACCEPT it across every format that can
    /// produce one, and the quad(s) must actually land (not get silently
    /// redirected to the default graph or dropped).
    #[test]
    fn blank_node_graph_targets_are_accepted_and_actually_land() {
        for (format_name, body) in [
            (
                "trig",
                r#"_:g1 { <urn:s> <urn:p> "trig-blank" . }"#.to_string(),
            ),
            (
                "application/n-quads",
                r#"<urn:s> <urn:p> "nquads-blank" _:g1 ."#.to_string(),
            ),
            (
                "n3",
                r#"<urn:s> <urn:p> { <urn:a> <urn:b> "n3-formula-blank" } ."#.to_string(),
            ),
        ] {
            assert!(
                validate_rdf_dataset_graph_targets("graph-a", &body, format_name, None).is_ok(),
                "{format_name}: a blank-node graph target must never be treated as reserved"
            );

            let store = Store::new().unwrap();
            load_rdf_dataset_into_store(&store, "graph-a", &body, format_name, None)
                .unwrap_or_else(|error| {
                    panic!("{format_name}: blank-node-graph import must succeed: {error}")
                });
            assert!(
                store.len().unwrap() > 0,
                "{format_name}: the blank-node-graph quad(s) must actually land in the store"
            );
        }
    }

    #[test]
    fn sparql_query_shapes_are_executed_against_store() {
        let store = Store::new().unwrap();
        store
            .load_from_slice(
                RdfParser::from_format(parse_rdf_format("nt").unwrap()),
                br#"<urn:s> <urn:p> "value" ."#,
            )
            .unwrap();

        let select = execute_sparql_query(
            &store,
            r#"SELECT ?s ?o WHERE { ?s <urn:p> ?o } ORDER BY ?s"#,
        )
        .unwrap();
        assert_eq!(select.result_type, "solutions");
        assert_eq!(select.variables, vec!["s".to_string(), "o".to_string()]);
        assert_eq!(select.rows.len(), 1);
        assert_eq!(select.rows[0].get("s").map(String::as_str), Some("<urn:s>"));
        assert_eq!(
            select.rows[0].get("o").map(String::as_str),
            Some("\"value\"")
        );
        assert_eq!(select.quad_count, 1);
        assert!(select.warnings.is_none(), "rows are nonempty; no warning");

        let ask = execute_sparql_query(&store, r#"ASK { <urn:s> <urn:p> "value" }"#).unwrap();
        assert_eq!(ask.result_type, "boolean");
        assert_eq!(ask.boolean, Some(true));

        let construct = execute_sparql_query(
            &store,
            r#"CONSTRUCT { <urn:s> <urn:p> ?o } WHERE { <urn:s> <urn:p> ?o }"#,
        )
        .unwrap();
        assert_eq!(construct.result_type, "graph");
        let graph = construct.graph.unwrap();
        assert!(graph.contains("<urn:s>"));
        assert!(graph.contains("<urn:p>"));
        assert!(graph.contains("\"value\""));
    }

    #[test]
    fn rdf_load_can_target_named_user_graph() {
        let store = Store::new().unwrap();
        load_rdf_into_store(
            &store,
            r#"<urn:s> <urn:p> "value" ."#,
            "turtle",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("load RDF into named graph");

        let result = execute_sparql_query(
            &store,
            r#"
SELECT ?o WHERE {
  GRAPH <urn:mnemosyne:local:graph:graph-a:user:rdf> {
    <urn:s> <urn:p> ?o .
  }
}
"#,
        )
        .expect("query named graph");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("o").map(String::as_str),
            Some("\"value\"")
        );
        assert!(
            result.warnings.is_none(),
            "query already names a GRAPH; no warning"
        );
    }

    #[test]
    fn rdf_dataset_load_preserves_named_graphs() {
        let store = Store::new().unwrap();
        load_rdf_dataset_into_store(
            &store,
            "graph-a",
            r#"<urn:s> <urn:p> "value" <urn:g> ."#,
            "application/n-quads",
            None,
        )
        .expect("load RDF dataset");

        let result = execute_sparql_query(
            &store,
            r#"
SELECT ?o WHERE {
  GRAPH <urn:g> {
    <urn:s> <urn:p> ?o .
  }
}
"#,
        )
        .expect("query named graph");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("o").map(String::as_str),
            Some("\"value\"")
        );
    }

    /// A9 unit-level pin (the full no-mock proof over a real gardend Store
    /// + the real `observatory` graph IRIs lives in
    /// `tests/observatory_authority.rs`'s Direction-6 family): a dataset
    /// payload that inline-names a `:projection:` graph must be refused
    /// BEFORE any quad reaches the store, across every dataset format that
    /// can carry a named graph at all.
    #[test]
    fn rdf_dataset_load_refuses_inline_reserved_projection_graph() {
        for (format, body) in [
            (
                "application/n-quads",
                r#"<urn:s> <urn:p> "value" <urn:mnemosyne:local:graph:graph-a:projection:obs:raw> ."#
                    .to_string(),
            ),
            (
                "trig",
                r#"<urn:mnemosyne:local:graph:graph-a:projection:obs:raw> { <urn:s> <urn:p> "value" . }"#
                    .to_string(),
            ),
        ] {
            let store = Store::new().unwrap();
            let error = load_rdf_dataset_into_store(&store, "graph-a", &body, format, None)
                .expect_err(&format!("{format}: reserved-graph dataset import must be refused"));
            assert!(
                error.contains("reserved projection graph"),
                "{format}: unexpected error shape: {error}"
            );
            assert_eq!(
                store.len().unwrap(),
                0,
                "{format}: a refused import must not leave a partial write behind"
            );
        }

        // Non-reserved control: the identical N-Quads shape retargeted at
        // `:user:rdf` is accepted and genuinely lands.
        let store = Store::new().unwrap();
        load_rdf_dataset_into_store(
            &store,
            "graph-a",
            r#"<urn:s> <urn:p> "value" <urn:mnemosyne:local:graph:graph-a:user:rdf> ."#,
            "application/n-quads",
            None,
        )
        .expect("non-reserved dataset import must be accepted");
        assert_eq!(store.len().unwrap(), 1);
    }

    /// MIXED dataset: non-reserved quads BEFORE and AFTER the one reserved
    /// quad, for every dataset-carrying format. Proves the scan does not stop
    /// early / get fooled by "the first or last quad looked fine" — the
    /// WHOLE import is refused, and NOTHING lands (not even the legitimate
    /// quads that came before the reserved one), for TriG, N-Quads, and
    /// JSON-LD alike.
    #[test]
    fn mixed_dataset_with_a_reserved_quad_sandwiched_between_non_reserved_quads_is_wholly_refused()
    {
        let cases: [(&str, String); 3] = [
            (
                "application/n-quads",
                [
                    r#"<urn:s1> <urn:p> "before" <urn:mnemosyne:local:graph:graph-a:user:rdf> ."#,
                    r#"<urn:s2> <urn:p> "reserved" <urn:mnemosyne:local:graph:graph-a:projection:obs:raw> ."#,
                    r#"<urn:s3> <urn:p> "after" <urn:mnemosyne:local:graph:graph-a:user:rdf> ."#,
                ]
                .join("\n"),
            ),
            (
                "trig",
                r#"
<urn:mnemosyne:local:graph:graph-a:user:rdf> { <urn:s1> <urn:p> "before" . }
<urn:mnemosyne:local:graph:graph-a:projection:obs:raw> { <urn:s2> <urn:p> "reserved" . }
<urn:mnemosyne:local:graph:graph-a:user:rdf> { <urn:s3> <urn:p> "after" . }
"#
                .to_string(),
            ),
            (
                "jsonld",
                r#"[
  { "@id": "urn:mnemosyne:local:graph:graph-a:user:rdf", "@graph": [ { "@id": "urn:s1", "urn:p": [{"@value": "before"}] } ] },
  { "@id": "urn:mnemosyne:local:graph:graph-a:projection:obs:raw", "@graph": [ { "@id": "urn:s2", "urn:p": [{"@value": "reserved"}] } ] },
  { "@id": "urn:mnemosyne:local:graph:graph-a:user:rdf", "@graph": [ { "@id": "urn:s3", "urn:p": [{"@value": "after"}] } ] }
]"#
                .to_string(),
            ),
        ];

        for (format, body) in cases {
            assert!(
                validate_rdf_dataset_graph_targets("graph-a", &body, format, None).is_err(),
                "{format}: a payload with ANY reserved-target quad must be refused, \
                 regardless of where it sits among non-reserved quads"
            );

            let store = Store::new().unwrap();
            let error = load_rdf_dataset_into_store(&store, "graph-a", &body, format, None)
                .expect_err(&format!(
                    "{format}: mixed reserved/non-reserved import must refuse"
                ));
            assert!(
                error.contains("reserved projection graph"),
                "{format}: unexpected error shape: {error}"
            );
            assert_eq!(
                store.len().unwrap(),
                0,
                "{format}: NEITHER the before- nor the after-quad may land — the whole \
                 import is refused, not just the offending quad"
            );
        }
    }

    /// Base-relative graph target: the payload names its reserved graph via
    /// an EMPTY relative reference (`<>`, which RFC3986 resolves to the base
    /// IRI itself) rather than spelling the reserved IRI out literally. The
    /// pre-check must resolve it (using the SAME `base_iri` the real load
    /// would use) before comparing against `is_reserved_rdf_graph_iri` — a
    /// byte-string match on the UNRESOLVED payload would miss this.
    #[test]
    fn base_relative_graph_target_resolves_to_a_reserved_iri_and_is_refused() {
        let reserved = "urn:mnemosyne:local:graph:graph-a:projection:obs:raw";
        let body = r#"<> { <urn:s> <urn:p> "relative" . }"#;

        let error = validate_rdf_dataset_graph_targets("graph-a", body, "trig", Some(reserved))
            .expect_err("a base-relative reference resolving to a reserved graph must refuse");
        assert!(error.contains("reserved projection graph"), "{error}");

        let store = Store::new().unwrap();
        load_rdf_dataset_into_store(&store, "graph-a", body, "trig", Some(reserved))
            .expect_err("the store-write primitive must also refuse the resolved target");
        assert_eq!(store.len().unwrap(), 0);

        // Non-reserved control: the identical relative-reference SHAPE,
        // resolved against a non-reserved base, is accepted and lands.
        let user_rdf = "urn:mnemosyne:local:graph:graph-a:user:rdf";
        let control_store = Store::new().unwrap();
        load_rdf_dataset_into_store(&control_store, "graph-a", body, "trig", Some(user_rdf))
            .expect("the identical relative shape against a non-reserved base must land");
        assert_eq!(control_store.len().unwrap(), 1);
    }

    /// Unicode-escaped IRI: the reserved graph's colon is spelled with a
    /// `\uXXXX` UCHAR escape inside the IRIREF rather than a literal `:`. The
    /// pre-check parses (not byte-matches) the payload, so the decoded IRI
    /// string is compared — the SAME string `is_reserved_rdf_graph_iri`
    /// would reject if it had been written out literally.
    #[test]
    fn unicode_escaped_reserved_graph_iri_is_refused() {
        // The reserved graph IRI with every ':' rewritten as its UCHAR
        // escape (`:`) — an IRIREF the payload's bytes never spell the
        // literal reserved string in, but that decodes to EXACTLY it.
        let literal_graph = "urn:mnemosyne:local:graph:graph-a:projection:obs:raw";
        let escaped_graph = literal_graph.replace(':', "\\u003A");
        assert_ne!(
            escaped_graph, literal_graph,
            "sanity: the escaped form must not equal the literal string byte-for-byte"
        );
        let body = format!(r#"<urn:s> <urn:p> "value" <{escaped_graph}> ."#);

        let error =
            validate_rdf_dataset_graph_targets("graph-a", &body, "application/n-quads", None)
                .expect_err(
                    "a unicode-escaped reserved graph IRI must still be recognized reserved",
                );
        assert!(error.contains("reserved projection graph"), "{error}");

        let store = Store::new().unwrap();
        load_rdf_dataset_into_store(&store, "graph-a", &body, "application/n-quads", None)
            .expect_err("the store-write primitive must also refuse the decoded target");
        assert_eq!(store.len().unwrap(), 0);
    }

    /// Compact/prefixed JSON-LD: the reserved graph's `@id` is expressed as a
    /// `@context`-prefixed compact IRI (`obs:raw`), not the JSON-LD dataset
    /// `@id`/`@graph` shape the round-trip fixture above always produces.
    /// Proves the check catches reserved-ness after JSON-LD's OWN term
    /// expansion, not just in the narrow shape oxigraph's serializer happens
    /// to emit.
    #[test]
    fn compact_prefixed_jsonld_reserved_graph_is_refused() {
        let body = r#"{
  "@context": { "obs": "urn:mnemosyne:local:graph:graph-a:projection:obs:", "ex": "urn:sophia:observatory:" },
  "@graph": [
    { "@id": "obs:raw", "@graph": [ { "@id": "ex:s", "ex:p": "compact-jsonld" } ] }
  ]
}"#;

        let error = validate_rdf_dataset_graph_targets("graph-a", body, "jsonld", None)
            .expect_err("a compact/prefixed JSON-LD reserved graph target must be refused");
        assert!(error.contains("reserved projection graph"), "{error}");

        let store = Store::new().unwrap();
        load_rdf_dataset_into_store(&store, "graph-a", body, "jsonld", None)
            .expect_err("the store-write primitive must also refuse the expanded target");
        assert_eq!(store.len().unwrap(), 0);
    }

    /// Malformed-AFTER-valid: the payload's first quad is syntactically fine
    /// and targets a NON-reserved graph, but a later line is unparseable.
    /// FAILS CLOSED per the house rule — refuse rather than admit — so even
    /// though the parser already yielded one good quad before hitting the
    /// error, NOTHING may land: this is a `ParseFailure`, not a partial
    /// success.
    #[test]
    fn malformed_quad_after_a_valid_quad_fails_closed_and_lands_nothing() {
        let body = "<urn:s> <urn:p> \"valid\" <urn:mnemosyne:local:graph:graph-a:user:rdf> .\nTHIS IS NOT VALID NQUADS SYNTAX\n";

        let outcome =
            classify_rdf_dataset_graph_targets("graph-a", body, "application/n-quads", None)
                .expect_err("a syntax error later in the payload must refuse the whole import");
        assert!(
            !outcome.is_reserved(),
            "a syntax error is a ParseFailure, not a Reserved-target refusal"
        );

        let store = Store::new().unwrap();
        load_rdf_dataset_into_store(&store, "graph-a", body, "application/n-quads", None)
            .expect_err("the store-write primitive must also refuse a malformed payload");
        assert_eq!(
            store.len().unwrap(),
            0,
            "the one syntactically-valid leading quad must NOT land when a later quad is malformed"
        );
    }

    /// `classify_rdf_dataset_graph_targets` must distinguish WHY it refused:
    /// a reserved target is `Reserved` (→ `AppError::validation`/HTTP 400 at
    /// the service layer), while a malformed payload (bad format name, bad
    /// base IRI, or a syntax error) is `ParseFailure` (→ `AppError::rdf`/HTTP
    /// 500) — restoring the "genuinely distinct" contract the doc comments
    /// claim (review r1 finding).
    #[test]
    fn classify_rdf_dataset_graph_targets_distinguishes_reserved_from_parse_failure() {
        let reserved = classify_rdf_dataset_graph_targets(
            "graph-a",
            r#"<urn:s> <urn:p> "v" <urn:mnemosyne:local:graph:graph-a:projection:obs:raw> ."#,
            "application/n-quads",
            None,
        )
        .expect_err("reserved target must refuse");
        assert!(
            reserved.is_reserved(),
            "a reserved target must classify as Reserved"
        );

        let bad_format =
            classify_rdf_dataset_graph_targets("graph-a", "irrelevant", "not-a-real-format", None)
                .expect_err("an unknown format name must refuse");
        assert!(
            !bad_format.is_reserved(),
            "an unrecognized format name is a ParseFailure, not Reserved"
        );

        // N-Quads has no concept of a base IRI at all (RDF 1.1 N-Quads
        // requires every IRI absolute) — oxigraph's own `with_base_iri`
        // silently no-ops for `RdfFormat::NQuads`/`NTriples` rather than
        // validating (verified above; not a security gap, since N-Quads
        // never resolves a relative graph IRI against it either way). TriG
        // DOES validate its base IRI, so use it here to exercise a genuine
        // `ParseFailure` from a malformed base IRI.
        let bad_base = classify_rdf_dataset_graph_targets(
            "graph-a",
            r#"<urn:s> <urn:p> "v" ."#,
            "trig",
            Some("not a valid base iri"),
        )
        .expect_err("an invalid base IRI must refuse");
        assert!(
            !bad_base.is_reserved(),
            "an invalid base IRI is a ParseFailure, not Reserved"
        );

        let bad_syntax = classify_rdf_dataset_graph_targets(
            "graph-a",
            "THIS IS NOT VALID NQUADS SYNTAX",
            "application/n-quads",
            None,
        )
        .expect_err("unparseable data must refuse");
        assert!(
            !bad_syntax.is_reserved(),
            "a syntax error is a ParseFailure, not Reserved"
        );

        // Non-reserved, well-formed control: no refusal at all.
        assert!(classify_rdf_dataset_graph_targets(
            "graph-a",
            r#"<urn:s> <urn:p> "v" <urn:mnemosyne:local:graph:graph-a:user:rdf> ."#,
            "application/n-quads",
            None,
        )
        .is_ok());
    }

    /// Default-graph acceptance INSIDE a dataset format — not the "whole
    /// payload has no named-graph syntax at all" bucket (already covered by
    /// `observatory_authority.rs`'s Direction-6 non-dataset-formats test),
    /// but a dataset-carrying format's OWN default-graph mapping: a TriG
    /// triple living OUTSIDE any `{ }` graph block, a 3-column N-Quads line
    /// (graph term omitted), and a top-level JSON-LD document with no
    /// `@graph` dataset wrapper at all. All three must be accepted AND the
    /// quad must actually be queryable back out of the store's default graph
    /// — not merely "the call returned Ok".
    #[test]
    fn default_graph_quads_inside_dataset_formats_actually_land() {
        let cases: [(&str, &str); 3] = [
            ("trig", r#"<urn:s> <urn:p> "default-in-trig" ."#),
            (
                "application/n-quads",
                r#"<urn:s> <urn:p> "default-in-nquads" ."#,
            ),
            (
                "jsonld",
                r#"{ "@id": "urn:s", "urn:p": [{"@value": "default-in-jsonld"}] }"#,
            ),
        ];

        for (format, body) in cases {
            assert!(
                validate_rdf_dataset_graph_targets("graph-a", body, format, None).is_ok(),
                "{format}: a default-graph quad can never be reserved; must be accepted"
            );

            let store = Store::new().unwrap();
            load_rdf_dataset_into_store(&store, "graph-a", body, format, None).unwrap_or_else(
                |error| panic!("{format}: default-graph import must succeed: {error}"),
            );

            let result = execute_sparql_query(&store, r#"ASK { <urn:s> <urn:p> ?o }"#)
                .expect("bare ASK against the default graph executes");
            assert_eq!(
                result.boolean,
                Some(true),
                "{format}: the quad must be queryable back out of the store's DEFAULT graph"
            );
        }
    }

    #[test]
    fn rdf_dump_graph_formats_can_default_to_named_user_graph() {
        let store = Store::new().unwrap();
        load_rdf_into_store(
            &store,
            r#"<urn:s> <urn:p> "value" ."#,
            "turtle",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("load RDF into named graph");

        let dump = dump_rdf_from_store(
            &store,
            "n-triples",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("dump named graph as triples");

        assert!(dump.data.contains("<urn:s> <urn:p> \"value\""));
    }

    #[test]
    fn bare_select_against_named_graph_only_store_warns_about_the_silent_zero() {
        let store = Store::new().unwrap();
        // All quads live under a named graph — nothing in the store default
        // graph — mirroring how gardend partitions every graph's `user:rdf`.
        load_rdf_into_store(
            &store,
            r#"<urn:s> <urn:p> "value" ."#,
            "turtle",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("load RDF into named graph");

        // Bare SELECT: no GRAPH clause anywhere, so it only ever sees the
        // (empty) default graph — a silent zero rather than a query error.
        let result = execute_sparql_query(&store, r#"SELECT ?o WHERE { <urn:s> <urn:p> ?o }"#)
            .expect("bare query executes");

        assert!(result.rows.is_empty());
        assert_eq!(result.quad_count, 1);
        let warnings = result.warnings.expect("silent zero should be flagged");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("named-graph-partitioned"));
        assert!(warnings[0].contains('1'), "warning cites the quad count");
        assert!(warnings[0].contains("GRAPH ?g"));
    }

    #[test]
    fn bare_select_against_a_genuinely_empty_store_does_not_warn() {
        // quad_count == 0: there is nothing hidden in a named graph, so the
        // zero rows are not "silent" — no warning should fire.
        let store = Store::new().unwrap();

        let result = execute_sparql_query(&store, r#"SELECT ?o WHERE { <urn:s> <urn:p> ?o }"#)
            .expect("bare query executes against an empty store");

        assert!(result.rows.is_empty());
        assert_eq!(result.quad_count, 0);
        assert!(result.warnings.is_none());
    }

    #[test]
    fn bare_select_with_no_matches_but_nonzero_quads_still_warns_case_insensitively() {
        let store = Store::new().unwrap();
        load_rdf_into_store(
            &store,
            r#"<urn:s> <urn:p> "value" ."#,
            "turtle",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("load RDF into named graph");

        // Lowercase "graph" as a plain identifier substring does not count —
        // only checked for completeness that the heuristic is on the query
        // text as a whole, not just literal "GRAPH".
        let result = execute_sparql_query(&store, r#"select ?o where { <urn:s> <urn:p> ?o }"#)
            .expect("bare lowercase-keyword query executes");

        assert!(result.rows.is_empty());
        assert!(result.warnings.is_some());
    }

    #[test]
    fn ask_and_construct_never_carry_the_silent_zero_warning() {
        let store = Store::new().unwrap();
        load_rdf_into_store(
            &store,
            r#"<urn:s> <urn:p> "value" ."#,
            "turtle",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("load RDF into named graph");

        // Neither shape's `rows` field carries the named-graph signal (it's
        // always empty for both), so neither should ever emit the warning
        // even though the store is named-graph-partitioned with quads.
        let ask = execute_sparql_query(&store, r#"ASK { <urn:s> <urn:p> "value" }"#)
            .expect("bare ASK executes");
        assert!(ask.warnings.is_none());

        let construct = execute_sparql_query(
            &store,
            r#"CONSTRUCT { <urn:s> <urn:p> ?o } WHERE { <urn:missing> <urn:p> ?o }"#,
        )
        .expect("bare CONSTRUCT executes");
        assert!(construct.warnings.is_none());
    }

    // ── the tokenization fix: literals/comments/IRIs merely CONTAINING the
    // word "graph" must not be mistaken for a real `GRAPH` clause (a false
    // negative that would silently SUPPRESS the silent-zero warning). ──

    #[test]
    fn strip_sparql_literals_and_comments_removes_quoted_and_commented_text() {
        let no_graph_word = |s: &str| !s.to_ascii_uppercase().contains("GRAPH");

        // The word "GRAPH" living only inside a string literal disappears.
        let stripped =
            strip_sparql_literals_and_comments(r#"SELECT ?o WHERE { ?s <urn:p> "GRAPH" }"#);
        assert!(no_graph_word(&stripped), "{stripped}");
        // Real query structure survives (the FILTER-less SELECT skeleton).
        assert!(stripped.contains("SELECT ?o WHERE"));
        assert!(stripped.contains('}'));

        // A `#`-comment mentioning GRAPH disappears, but content AFTER the
        // newline (the real query) survives untouched.
        let stripped =
            strip_sparql_literals_and_comments("SELECT ?o # GRAPH note\nWHERE { ?s ?p ?o }");
        assert!(no_graph_word(&stripped), "{stripped}");
        assert!(stripped.contains("WHERE { ?s ?p ?o }"));

        // An escaped quote inside a string literal does not end it early —
        // the embedded GRAPH still disappears, not just the prefix up to the
        // escaped quote.
        let stripped = strip_sparql_literals_and_comments(r#""a \" GRAPH \" b""#);
        assert!(no_graph_word(&stripped), "{stripped}");

        // Triple-quoted literal.
        let stripped = strip_sparql_literals_and_comments("'''multi GRAPH line'''");
        assert!(no_graph_word(&stripped), "{stripped}");
    }

    #[test]
    fn query_mentions_graph_keyword_ignores_the_word_inside_a_string_literal() {
        assert!(!query_mentions_graph_keyword(
            r#"SELECT ?o WHERE { ?s <urn:p> "this graph rocks" }"#
        ));
        assert!(query_mentions_graph_keyword(
            r#"SELECT ?o WHERE { GRAPH ?g { ?s <urn:p> ?o } }"#
        ));
    }

    #[test]
    fn query_mentions_graph_keyword_ignores_the_word_inside_a_comment() {
        assert!(!query_mentions_graph_keyword(
            "# GRAPH scoping TODO\nSELECT ?o WHERE { ?s <urn:p> ?o }"
        ));
    }

    #[test]
    fn query_mentions_graph_keyword_ignores_the_word_inside_an_iri() {
        assert!(!query_mentions_graph_keyword(
            "SELECT ?o WHERE { ?s <http://example.test/graph#Thing> ?o }"
        ));
    }

    #[test]
    fn bare_select_with_graph_word_only_in_a_string_literal_still_warns() {
        let store = Store::new().unwrap();
        load_rdf_into_store(
            &store,
            r#"<urn:s> <urn:p> "value" ."#,
            "turtle",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("load RDF into named graph");

        // No real GRAPH clause — the word only lives inside a string literal.
        // Before the tokenization fix, the naive substring scan treated this
        // as "mentions GRAPH" and wrongly suppressed the warning.
        let result = execute_sparql_query(
            &store,
            r#"SELECT ?o WHERE { <urn:s> <urn:p> ?o . FILTER(?o != "this graph rocks") }"#,
        )
        .expect("bare query with a literal executes");
        assert!(result.rows.is_empty());
        let warnings = result
            .warnings
            .expect("silent zero must still be flagged despite the literal");
        assert!(warnings[0].contains("named-graph-partitioned"));
    }

    #[test]
    fn bare_select_with_graph_word_only_in_a_comment_still_warns() {
        let store = Store::new().unwrap();
        load_rdf_into_store(
            &store,
            r#"<urn:s> <urn:p> "value" ."#,
            "turtle",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("load RDF into named graph");

        let result = execute_sparql_query(
            &store,
            "# GRAPH scoping TODO\nSELECT ?o WHERE { <urn:s> <urn:p> ?o }",
        )
        .expect("bare commented query executes");
        assert!(result.rows.is_empty());
        let warnings = result
            .warnings
            .expect("silent zero must still be flagged despite the comment");
        assert!(warnings[0].contains("named-graph-partitioned"));
    }

    #[test]
    fn query_with_a_real_graph_clause_and_an_unrelated_comment_suppresses_the_warning() {
        let store = Store::new().unwrap();
        load_rdf_into_store(
            &store,
            r#"<urn:s> <urn:p> "value" ."#,
            "turtle",
            None,
            Some("urn:mnemosyne:local:graph:graph-a:user:rdf"),
        )
        .expect("load RDF into named graph");

        // A comment mentions "graph" too, but a REAL GRAPH clause is also
        // present — the warning must still be suppressed (no regression).
        let result = execute_sparql_query(
            &store,
            "# not a graph clause, just a note\n\
             SELECT ?o WHERE { GRAPH <urn:mnemosyne:local:graph:graph-a:user:rdf> { <urn:s> <urn:p> ?o } }",
        )
        .expect("query with a real GRAPH clause executes");
        assert_eq!(result.rows.len(), 1);
        assert!(result.warnings.is_none());
    }
}
