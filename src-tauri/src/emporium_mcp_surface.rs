//! Thin MCP handlers over Emporium's existing engine surface (T0 items 2-3):
//! `emporium_list` / `emporium_read` wrap the generic object reads
//! ([`crate::emporium::objects::list_objects`] / `read_object`); `emporium_heads`
//! / `emporium_sweep` wrap the §8.1 reconciliation reader/recompute
//! ([`crate::emporium::sweep::current_heads_flagged`] / `sweep_memory_conformance`).
//! `sparql_query_named` (T2 item 11) wraps the query-face engine
//! ([`crate::emporium::query_engine::run_named_query`]). `emporium_query`
//! (T2 item 11a) wraps the OBJECT query face
//! ([`crate::emporium::object_query::run_object_query`]) — "objects out, not
//! rows": criteria in, hydrated shaped objects out, JSON or Markdown faced.
//!
//! Lives at the crate root (not inside `emporium/`) because every function it
//! calls is already `pub(crate)` — no `emporium`-internal (`pub(super)`) access
//! is needed here (that's what `emporium_violations` in `emporium/mcp.rs` is
//! for, since `open_memory_store` is scoped to the `emporium` module tree).
//!
//! READ ONLY this wave: `create_objects` / `update_object` / `delete_object`
//! (the object surface's write/retract) are deliberately DEFERRED — see the
//! T0 plan. `emporium_list` / `emporium_read` are pure reads; `emporium_heads`
//! is a pure read (`rdf.query`); `emporium_sweep` MUTATES the violation ledger
//! projection (`rdf.update`) even though it never touches the memory records
//! themselves.

use serde_json::Value;

use crate::app_error::{AppError, AppResult};
use crate::app_error_codes;
use crate::app_runtime::AppHandle;
use crate::emporium::object_query::{run_object_query, ObjectQueryOptions};
use crate::emporium::objects::{list_objects, read_object, ObjectError};
use crate::emporium::query_emit::QueryEmitError;
use crate::emporium::query_engine::run_named_query;
use crate::emporium::sweep::{current_heads_flagged, sweep_memory_conformance};
use crate::emporium::vocab_routes::render_object_query_markdown;
use crate::emporium::write::{emporium_retract_with_identity, emporium_write};
use crate::mcp_arg_utils::{mcp_arg_bool, mcp_arg_string, mcp_arg_usize, mcp_required_graph_id};

/// Map the object surface's HTTP-shaped [`ObjectError`] onto [`AppError`] —
/// the same status-family mapping `emporium/vocab_routes.rs`'s
/// `object_error_response` uses for the HTTP face, so the MCP face agrees.
fn object_error_to_app_error(error: ObjectError) -> AppError {
    let message = error.message().to_string();
    match error {
        ObjectError::BadRequest(_) => AppError::validation(message),
        // Vocab/class misconfiguration — NEVER coded, so a client cannot
        // collapse a setup error into a quiet empty card.
        ObjectError::NotFound(_) => AppError::not_found(message),
        // Genuine absence of an object in a well-formed class — the ONLY
        // NotFound-family variant that carries `object_not_found`.
        ObjectError::Absent(_) => {
            AppError::not_found(message).with_code(app_error_codes::OBJECT_NOT_FOUND)
        }
        ObjectError::Conflict(_) => AppError::conflict(message),
        ObjectError::Internal(_) => AppError::internal(message),
    }
}

fn required_field(arguments: &Value, key: &str) -> AppResult<String> {
    mcp_arg_string(arguments, &[key])
        .ok_or_else(|| AppError::validation(format!("{key} is required")))
}

/// `emporium_list` — LIST the subjects of one class in a vocab's projection
/// sink, paginated. Required `graph_id`/`graphId`, `vocab`, `class`; optional
/// `limit` (default 100, the engine itself clamps to 1..=500) and `offset`
/// (default 0).
pub(crate) fn mcp_local_emporium_list(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let vocab = required_field(arguments, "vocab")?;
    let class = required_field(arguments, "class")?;
    let limit = mcp_arg_usize(arguments, &["limit"], 100);
    let offset = mcp_arg_usize(arguments, &["offset"], 0);
    list_objects(&app, &graph_id, &vocab, &class, limit, offset).map_err(object_error_to_app_error)
}

/// `emporium_read` — READ one object's full predicate span. Required
/// `graph_id`/`graphId`, `vocab`, `class`, `address` (a full subject IRI, or a
/// bare `localId` minted through the class's `subject_rule`).
pub(crate) fn mcp_local_emporium_read(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let vocab = required_field(arguments, "vocab")?;
    let class = required_field(arguments, "class")?;
    let address = required_field(arguments, "address")?;
    read_object(&app, &graph_id, &vocab, &class, &address).map_err(object_error_to_app_error)
}

/// `emporium_heads` — the ratified "return all heads flagged" read (§8.1
/// contested-by-default): every current memory head, grouped by lineage, each
/// lineage flagged `contested` when it has >1 head. A pure read; storage never
/// picks a winner. Required `graph_id`/`graphId`; optional `observer` (a
/// per-agent membrane; default = the shared commons).
pub(crate) fn mcp_local_emporium_heads(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let observer = mcp_arg_string(arguments, &["observer"]).unwrap_or_default();
    let report = current_heads_flagged(&app, &graph_id, &observer).map_err(AppError::internal)?;
    Ok(serde_json::json!(report))
}

/// `emporium_sweep` — the §8.1 post-hoc conformance sweep: detects contested
/// lineages (>1 `mem:isCurrent` head sharing a `mem:lineage`) over the LIVE
/// memory projection and files each finding into the violation ledger as this
/// sweep's advisory testimony. MUTATES the ledger projection (never the memory
/// records themselves) — scoped `rdf.update`, not `rdf.query`. Required
/// `graph_id`/`graphId`; optional `observer` (default = the shared commons).
pub(crate) fn mcp_local_emporium_sweep(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let observer = mcp_arg_string(arguments, &["observer"]).unwrap_or_default();
    let report =
        sweep_memory_conformance(&app, &graph_id, &observer).map_err(AppError::internal)?;
    Ok(serde_json::json!(report))
}

/// Map the query-face engine's [`QueryEmitError`] onto [`AppError`] — the same
/// status-family shape as [`object_error_to_app_error`] above (this MCP tool
/// sits directly beside the HTTP query-catalog face, which maps the same
/// error type via `loopback_error`).
fn query_emit_error_to_app_error(error: QueryEmitError) -> AppError {
    let message = error.message().to_string();
    match error {
        QueryEmitError::BadRequest(_) => AppError::validation(message),
        QueryEmitError::NotFound(_) => AppError::not_found(message),
        QueryEmitError::Internal(_) => AppError::internal(message),
    }
}

/// `sparql_query_named` — T2 item 11's query-face substrate reachable from the
/// agent's hands: run one of a class's canonical named queries (`byId`,
/// `currentHeads`, `lineageOf`, `countBy`), DERIVED from the class's declared
/// shape + [`crate::emporium::contract::MaterializationSignature`] (never a
/// hand-maintained per-class query). Required `graph_id`/`graphId`, `vocab`,
/// `class`, `query_name`/`queryName`; `params` carries the query-specific
/// arguments (`subject` for byId/lineageOf, `asOf` for currentHeads, `attr`
/// for countBy); optional `observer` widens a membrane-ed class's scope to
/// commons ∪ that observer's membrane (RATIFIED: no observer ⇒ commons only).
/// Returns `{graphId, vocab, class, queryName, variables, rows, warnings?}` —
/// the `rdf_query_service` rows+warnings shape, generalized.
pub(crate) fn mcp_local_sparql_query_named(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let vocab = required_field(arguments, "vocab")?;
    let class = required_field(arguments, "class")?;
    let query_name = mcp_arg_string(arguments, &["query_name", "queryName"]).ok_or_else(|| {
        AppError::validation(
            "query_name is required (one of: byId, currentHeads, lineageOf, countBy)",
        )
    })?;
    let observer = mcp_arg_string(arguments, &["observer"]);
    let params = arguments
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let outcome = run_named_query(
        &app,
        &graph_id,
        &vocab,
        &class,
        &query_name,
        observer.as_deref(),
        &params,
    )
    .map_err(query_emit_error_to_app_error)?;
    Ok(serde_json::json!(outcome))
}

/// `emporium_query` — T2 item 11a, the OBJECT query face: "objects out, not
/// rows". Required `graph_id`/`graphId`, `vocab`, `class`, `criteria` (a JSON
/// object of `{predicateKey: value | {operator: value}}` — legal keys are the
/// class's own declared properties; legal operators are datatype-derived:
/// `eq` universally, `contains` for strings, `before`/`after` for dateTime).
/// Optional `observer` (commons ∪ that witness's membrane; RATIFIED default:
/// no observer ⇒ commons only), `perspectives` (only legal value `"all"` —
/// merges every discovered witness membrane, each result tagged
/// `witnessGraph`), `as_of`/`asOf` (epoch-millis or ISO-8601 — historical
/// heads for a contested class; rejected for a class with no lineage
/// convention), `face` (`"json"` default, or `"markdown"`/`"md"`),
/// `limit`/`cursor` (pagination; `cursor` is a stable-order offset this
/// wave). Returns hydrated, shaped objects (never bindings rows) —
/// contested-by-declaration classes are flagged with sibling head refs.
pub(crate) fn mcp_local_emporium_query(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let vocab = required_field(arguments, "vocab")?;
    let class = required_field(arguments, "class")?;
    let criteria = arguments
        .get("criteria")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let observer = mcp_arg_string(arguments, &["observer"]);
    let perspectives = mcp_arg_string(arguments, &["perspectives"]);
    let as_of = arguments
        .get("as_of")
        .or_else(|| arguments.get("asOf"))
        .cloned();
    let limit = arguments
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let cursor = arguments
        .get("cursor")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .map(|n| n as usize);
    let face = mcp_arg_string(arguments, &["face"]).unwrap_or_else(|| "json".to_string());

    let options = ObjectQueryOptions {
        observer,
        perspectives,
        as_of,
        limit,
        cursor,
    };
    let outcome = run_object_query(&app, &graph_id, &vocab, &class, &criteria, &options)
        .map_err(query_emit_error_to_app_error)?;

    match face.to_ascii_lowercase().as_str() {
        "markdown" | "md" => Ok(serde_json::json!({
            "graphId": outcome.graph_id,
            "vocab": outcome.vocab,
            "class": outcome.class,
            "face": "markdown",
            "markdown": render_object_query_markdown(&outcome),
        })),
        "json" => Ok(serde_json::json!(outcome)),
        other => Err(AppError::validation(format!(
            "emporium_query: unknown face '{other}' — legal values: json, markdown, md"
        ))),
    }
}

/// `emporium_write` — T-W item 1, the MCP write surface over the existing
/// spine (the five laws — see [`crate::emporium::write`]'s module doc for the
/// mechanical detail). Required `graph_id`/`graphId`, `vocab`, `records`
/// (non-empty array); optional `dry_run`/`dryRun` (validation preview, writes
/// nothing), `publish` (explicit commons for a membrane-ed vocab), `observer`
/// (the writer's own membrane; per-record `observerAgentId` overrides it).
/// Returns `{graphId, vocab, dryRun, ok, results:[{subject, outcome:
/// applied|converged|halted, flags?}], journalRef?, warnings, violations?}`.
pub(crate) async fn mcp_local_emporium_write(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let vocab = required_field(arguments, "vocab")?;
    let records: Vec<Value> = arguments
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            AppError::validation("records is required (a non-empty array)".to_string())
        })?;
    let dry_run = mcp_arg_bool(arguments, &["dry_run", "dryRun"], false);
    let publish = mcp_arg_bool(arguments, &["publish"], false);
    let observer = mcp_arg_string(arguments, &["observer"]);
    let expected = crate::graph_incarnation_admission::expected_incarnation(arguments)?;
    let source_gate = crate::source_sync::acquire_source_gate(&graph_id).await;
    if !dry_run && crate::source_sync::source_authority_active(&app, &graph_id)? {
        drop(source_gate);
        return crate::source_sync::mcp_source_emporium_write(app, arguments).await;
    }
    if !dry_run && expected.is_some() {
        return Err(AppError::validation(
            "expected-incarnation emporium_write requires active source authority; call source_pull before writing",
        ));
    }
    let _identity_lease = crate::graph_incarnation_admission::acquire_expected_lifetime(
        &app, &graph_id, expected.as_deref(),
    ).await?;
    // Ludus events derive their append-only identity from the source ledger,
    // not from generic projection upsert. Never accept a legacy write that
    // would bypass that authority; explicit source_pull establishes it.
    // Validation previews remain available and other packs are unchanged.
    if !dry_run && vocab == "ludus-core" {
        return Err(AppError::validation(
            "ludus-core writes require active source authority; call source_pull for this graph before writing",
        ));
    }
    let result = emporium_write(
        &app,
        &graph_id,
        &vocab,
        &records,
        dry_run,
        publish,
        observer.as_deref(),
    )
    .await
    .map_err(object_error_to_app_error);
    drop(source_gate);
    result
}

/// `emporium_retract` — T-W item 2, Law 4: mints a retraction EVENT on
/// `subject` (who/when/rationale) — heads exclude it, faces hide it, the log
/// keeps it. NO hard-delete (that stays a separate, deferred admin-tier act).
/// Required `graph_id`/`graphId`, `subject` (a full IRI), `rationale`;
/// optional `kind` (`"retract"` default, or `"archive"`), `observer` (the
/// witness recording the retraction — NOT used to locate the subject, which
/// is found by a store-wide scan since this tool takes no vocab/class).
pub(crate) async fn mcp_local_emporium_retract(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let subject = required_field(arguments, "subject")?;
    let rationale = required_field(arguments, "rationale")?;
    let kind = mcp_arg_string(arguments, &["kind"]).unwrap_or_else(|| "retract".to_string());
    let observer = mcp_arg_string(arguments, &["observer"]);
    let event_id = mcp_arg_string(
        arguments,
        &[
            "retraction_event_id",
            "retractionEventId",
            "operation_id",
            "operationId",
        ],
    );
    let at_ms = arguments
        .get("at_ms")
        .or_else(|| arguments.get("atMs"))
        .and_then(Value::as_i64);
    let source_gate = crate::source_sync::acquire_source_gate(&graph_id).await;
    if crate::source_sync::source_authority_active(&app, &graph_id)? {
        drop(source_gate);
        return crate::source_sync::mcp_source_emporium_retract(app, arguments).await;
    }
    let result = emporium_retract_with_identity(
        &app,
        &graph_id,
        &subject,
        &rationale,
        &kind,
        observer.as_deref(),
        event_id.as_deref(),
        at_ms,
    )
    .await
    .map_err(object_error_to_app_error);
    drop(source_gate);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-C23/E24's regression guard, R18 (master §8.3): a single `.with_code()`
    /// must not code the misconfiguration and genuine-absence arms alike.
    /// Master §4.2.1 names this `object_not_found_is_coded_only_for_absence`.
    /// Proved directly against the mapping function — it is pure, and the
    /// claim under test is about ITS logic, not the surrounding MCP dispatch.
    #[test]
    fn object_not_found_is_coded_only_for_absence() {
        let misconfigured = object_error_to_app_error(ObjectError::NotFound(
            "class 'Nope' is not declared by vocab 'v'".to_string(),
        ));
        assert_eq!(
            misconfigured.code(),
            None,
            "a vocab/class misconfiguration must never be coded — a client must not \
             collapse a real setup error into a quiet empty card"
        );

        let absent = object_error_to_app_error(ObjectError::Absent(
            "no object <urn:x> in <urn:sink>".to_string(),
        ));
        assert_eq!(absent.code(), Some(app_error_codes::OBJECT_NOT_FOUND));

        // Both arms share the same 404 status — the split is about
        // machine-readability, not about what the wire says happened.
        assert_eq!(
            misconfigured.kind(),
            crate::app_error::AppErrorKind::NotFound
        );
        assert_eq!(absent.kind(), crate::app_error::AppErrorKind::NotFound);
    }
}
