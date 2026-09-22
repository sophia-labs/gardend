//! REAL-store, REAL-gate proof surface for the `observatory` graph's
//! two-writer authority partition (§A.6).
//!
//! Every function here is a thin, literal delegate to the crate's actual
//! authority-gate and store-execution functions — `rdf_authority.rs` only
//! grew a visibility change for A9 (`is_reserved_rdf_graph_iri` widened from
//! module-private to `pub(crate)` so the dataset-import path could reuse it;
//! no logic changed), never edited beyond that. `pub` so
//! `tests/observatory_authority.rs` — a separate integration-test crate —
//! can drive the real gate in-process against a real on-disk Oxigraph store,
//! with no live gardend server required.

use crate::rdf_authority::{user_rdf_target_graph_iri, validate_sparql_update_authority};
use crate::rdf_query_service::{
    execute_sparql_update, load_rdf_dataset_into_store, load_rdf_into_store,
    validate_rdf_dataset_graph_targets,
};
use crate::rdf_store_service::open_graph_store;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use std::path::Path;
use std::sync::Arc;

/// Open a REAL on-disk gardend Oxigraph store at `graph_dir/store.oxigraph`
/// — the exact function `rdf_service.rs` opens for every RDF request
/// (`load_rdf_service`/`load_rdf_dataset_service`/`run_sparql_update_service`
/// all call it). No seeding (`ensure_graph_store_seeded`): the authority
/// gate is graph_id/IRI-string-driven and does not depend on any
/// pre-existing graph.json/document content, so a bare fresh store is a
/// faithful, minimal substrate for these tests.
pub fn open_real_store(graph_dir: &Path) -> Result<Arc<Store>, String> {
    open_graph_store(graph_dir)
}

/// The production authority check `run_sparql_update_service` runs before
/// EVER touching the store (`rdf_service.rs:142`,
/// `rdf_authority.rs:217-251`). `Err` iff the update is refused: a reserved
/// named-graph target, or an unconditionally-forbidden bare verb
/// (`CLEAR ALL`/`CLEAR NAMED`/`DROP ALL`/`DROP NAMED`, a variable
/// `GRAPH`/`WITH` target).
pub fn validate_sparql_update(graph_id: &str, update: &str) -> Result<(), String> {
    validate_sparql_update_authority(graph_id, update)
}

/// The production `load_rdf` target-resolution check
/// (`rdf_service.rs:210-213`, `rdf_authority.rs:200-215`) — returns the
/// resolved target graph IRI (defaulting to `:user:rdf` when
/// `requested_graph_iri` is `None`), or `Err` iff the requested graph is
/// reserved (caught by the same `is_reserved_rdf_graph_iri` the SPARQL
/// update path uses).
pub fn resolve_load_rdf_target(
    graph_id: &str,
    requested_graph_iri: Option<&str>,
) -> Result<String, String> {
    user_rdf_target_graph_iri(graph_id, requested_graph_iri)
}

/// Execute an already-authority-cleared SPARQL update directly against a
/// REAL store. The harness's non-triviality control: proves a given update
/// SHAPE is syntactically real and DOES write when its target is not
/// reserved, so a `validate_sparql_update` refusal on the reserved-target
/// twin of the same shape is provably about reserved-ness, not malformed
/// SPARQL.
pub fn execute_update_unchecked(store: &Store, update: &str) -> Result<usize, String> {
    execute_sparql_update(store, update).map(|result| result.quad_count)
}

/// `load_rdf`'s underlying store write (`rdf_service.rs:215-221`), called
/// directly — the harness's positive control proving `load_rdf_into_store`
/// itself is a real, working write path, so a `resolve_load_rdf_target`
/// refusal upstream is provably the only thing stopping a reserved-graph
/// `load_rdf`, not some unrelated failure. `data`/`format` follow
/// `parse_rdf_format` (e.g. `"nt"`, `"ttl"`); `target_graph_iri` is required
/// (mirrors `load_rdf_service` after target resolution has already run).
pub fn load_rdf_unchecked(
    store: &Store,
    data: &str,
    format: &str,
    target_graph_iri: &str,
) -> Result<usize, String> {
    load_rdf_into_store(store, data, format, None, Some(target_graph_iri))
        .map(|result| result.quad_count)
}

/// The production `load_rdf_dataset` PRE-CHECK (A9,
/// `rdf_service.rs::load_rdf_dataset_service`'s
/// `validate_rdf_dataset_graph_targets(..)?` step, run BEFORE the store is
/// even opened) — parses `data` as `format` and returns `Err` iff ANY named
/// graph the payload carries inline is reserved (the same
/// `is_reserved_rdf_graph_iri` predicate `validate_sparql_update`/
/// `resolve_load_rdf_target` gate on). The harness's Direction-6-family
/// counterpart to `validate_sparql_update`/`resolve_load_rdf_target`: proves
/// the SERVICE-LAYER gate refuses, independent of `load_rdf_dataset_unchecked`
/// proving the store-write primitive ALSO refuses on its own.
pub fn validate_rdf_dataset_targets(
    graph_id: &str,
    data: &str,
    format: &str,
) -> Result<(), String> {
    validate_rdf_dataset_graph_targets(graph_id, data, format, None)
}

/// `load_rdf_dataset`'s underlying store write (`rdf_service.rs:234-260`) —
/// dataset formats (TriG/N-Quads/JSON-LD) carry their OWN named-graph IRIs
/// inline in `data`. Pre-A9 nothing checked whether an inline graph IRI was a
/// reserved `:projection:*` target (Direction-6's original gap). Post-A9,
/// `load_rdf_dataset_into_store` itself scans every named graph the payload
/// carries and refuses the whole import if any is reserved — called
/// "_unchecked" because, like `load_rdf_unchecked`, it bypasses the SERVICE
/// scaffolding (`existing_graph_dir`/`ensure_graph_store_seeded`/
/// `touch_graph_updated_at`, and the service's own `AppError::validation`
/// pre-check `validate_rdf_dataset_graph_targets`), not because the
/// reserved-graph gate itself is off — that gate is now baked into the
/// store-write primitive this function calls directly, precisely so a
/// caller reaching the primitive by ANY path (service or this harness) stays
/// protected.
pub fn load_rdf_dataset_unchecked(
    store: &Store,
    graph_id: &str,
    data: &str,
    format: &str,
) -> Result<usize, String> {
    load_rdf_dataset_into_store(store, graph_id, data, format, None).map(|result| result.quad_count)
}

/// Did ANY quad land in `graph_iri` on `store`? A minimal `ASK`, run
/// directly against the real store via oxigraph's own public
/// `SparqlEvaluator` (not a reuse of `rdf_query_service::execute_sparql_query`,
/// whose result type is crate-private) — the harness's read-back oracle.
pub fn graph_has_any_quad(store: &Store, graph_iri: &str) -> Result<bool, String> {
    let query = format!("ASK {{ GRAPH <{graph_iri}> {{ ?s ?p ?o }} }}");
    let results = SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|error| format!("parse ASK: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute ASK: {error}"))?;
    match results {
        QueryResults::Boolean(value) => Ok(value),
        QueryResults::Solutions(_) => {
            Err("expected a boolean ASK result, got Solutions".to_string())
        }
        QueryResults::Graph(_) => Err("expected a boolean ASK result, got Graph".to_string()),
    }
}
