//! The CRDT applier — the gardend in-process write path (P3-WP1 + P3-WP2).
//!
//! Port of `emporium_engine/ingest/applier.py::apply_plan` (the loud-halt plan
//! executor). Where the Python twin crosses the platform-next gateway MCP
//! boundary (every folder/doc/wire mutation is a remote tool call), this gardend
//! twin maps each plan verb to the cell's **in-process CRDT surface**:
//!
//!   create_folder  -> enqueue `workspace.createFolder`  (label -> `name`)
//!   rename_folder  -> enqueue `workspace.updateFolder`  (label -> `name`)
//!   write_doc      -> enqueue `document.write` (content lands here; new docs
//!                     placed at creation via `parentId`); then read the doc back
//!                     to capture the first code-block id
//!   move           -> enqueue `workspace.moveDocuments` ({documentIds,parentId});
//!                     a move of a just-written doc is a recorded NO-OP
//!   create_wires   -> per wire: enqueue `workspace.createWire`
//!   delete_wires   -> per wireId: enqueue `workspace.deleteWire` (tolerate
//!                     "wire not found", matching applier.py's KeyError catch)
//!   sparql_update  -> resolve placeholders -> GRAPH-wrap into the user:rdf graph
//!                     -> `run_sparql_update_service`
//!
//! 🔴 READ-GRAPH == WRITE-GRAPH (risk #1): the sparql_update steps wrap into
//! `INSERT/DELETE DATA { GRAPH <{root}:user:rdf> { body } }` =
//! [`user_rdf_graph_iri`] — the SAME graph the survey reads. NEVER the bare
//! `<{root}>` (banned by `validate_sparql_update_authority`) and NEVER any
//! `:projection:` graph. The literal `:user:rdf` form passes the validator (its
//! `:user:rdf>` suffix differs from the banned bare-root `>`).
//!
//! 🔴 BLOCK-ID STABILITY (risk #3): `wf:scriptBlock` / `wf:textBlock`
//! idempotency depends on `document.write` preserving block-ids for unchanged
//! content. `document_write` keeps a block's id when its content is unchanged, so
//! a converged doc that is NOT rewritten this apply re-reads to recover the SAME
//! block URI. A two-apply of the same plan yields a byte-identical wf:scriptBlock
//! AND skips the second write (the planner's `doc_converged` drops the WriteDoc
//! step). See the `block_id_stability_two_apply` test.
//!
//! 🔴 LOUD-HALT: the executor halts on the FIRST failed step; the report names
//! the halted step (`ok=false`, `haltedAt`).
//!
//! Provenance (port of `_record_doc_provenance`): each `write_doc` REPLACES (never
//! accumulates) the managed-document provenance triples — `emp:intentSha256`,
//! `emp:renderSha256`, `emp:managedBy` — in the SAME user:rdf graph, hand-built
//! with `GRAPH` already inline (NOT passed through `graph_wrap`). `renderSha256`
//! is the sha of the markdown read back from what the write actually built, so it
//! is an applier-internal action paired with each write, not a planner op.

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde_json::{json, Value as Json};

use crate::app_error::AppError;
use crate::app_runtime::AppHandle;
use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
use crate::document_export_rendering::document_xml;
use crate::document_record_store::read_document_record;
use crate::emporium::asserts::AssertReport;
use crate::emporium::contract::VocabularyContract;
use crate::emporium::planner::{Plan, Step, WireSpec};
use crate::emporium::terms::{canonical_md, sha256_text, EMPORIUM_NS, PLACEHOLDER_NS};
use crate::paths::{document_dir, existing_graph_dir};
use crate::rdf::graph_subject;
use crate::rdf_authority::user_rdf_graph_iri;
use crate::rdf_service::{run_sparql_update_service, SparqlUpdateInput};

/// emp: provenance predicate locals (terms.py:45-47). EMPORIUM_NS lives in
/// terms.rs; the survey strips it back off on read.
const EMP_INTENT_SHA: &str = "intentSha256";
const EMP_RENDER_SHA: &str = "renderSha256";
const EMP_MANAGED_BY: &str = "managedBy";

/// First code-block id in a doc's XML render (applier.py:54, IGNORECASE). gardend
/// emits `<codeBlock data-block-id="block-XXXX">`; the `pre` alternation is a
/// harmless dead branch (gardend never emits a `pre` block type).
fn code_block_id_re() -> Regex {
    Regex::new(r#"(?i)<(?:codeBlock|pre)[^>]*data-block-id="([^"]+)""#)
        .expect("code-block-id regex is valid")
}

/// `urn:wf-emit:placeholder:NAME` occurrences inside a rendered SPARQL update.
fn placeholder_in_update_re() -> Regex {
    Regex::new(&format!(r#"{}([^>\s]+)"#, regex::escape(PLACEHOLDER_NS)))
        .expect("placeholder regex is valid")
}

/// `^(INSERT DATA|DELETE DATA)\s*\{` — the verb the planner's `render_updates`
/// always produces (terms.rs:337, NO graph clause).
fn verb_re() -> Regex {
    Regex::new(r"(?s)^(INSERT DATA|DELETE DATA)\s*\{").expect("verb regex is valid")
}

/// One executed step's report entry. Serializes camelCase like the Python
/// `report["steps"]` dicts; `extra` carries the per-op fields (folderId, docId,
/// count, placedAtCreation, …) and the ok/error outcome.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct StepReport {
    pub(crate) op: String,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Json>,
}

/// The apply report. Mirrors the Python `report` dict
/// (graph/workflow/mode/summary/warnings/steps/ok[/haltedAt]). `ok=false` with a
/// `haltedAt` index names the first failed step.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyReport {
    pub(crate) graph: String,
    pub(crate) workflow: String,
    pub(crate) mode: String,
    pub(crate) summary: Json,
    pub(crate) warnings: Vec<String>,
    pub(crate) steps: Vec<StepReport>,
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) halted_at: Option<usize>,
    /// The captured `wf:scriptBlock` URI (SCRIPT_BLOCK), if the workflow doc's
    /// code block was captured this apply. Surfaced for the block-id-stability
    /// assertion and for callers that want the concrete value without re-reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) script_block: Option<String>,
    /// The post-apply integrity assertion (workflow kind only). Folded in by the
    /// spine's `apply_and_assert` after a fresh re-survey; `None` until then (the
    /// applier itself does not assert) and JSON `null` for the campaign kind
    /// (campaign assertion is deferred — `report["assertion"] = None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) assertion: Option<AssertReport>,
}

/// Execute a plan against the gardend cell store. Loud halt on the first failed
/// step; the report names where (`ok=false`, `haltedAt`).
///
/// `generator_id` is the `emp:managedBy` value, `emporium:{name}@{version}`
/// (spine.py:265), built from the contract.
///
/// Every folder/doc/wire mutation crosses the in-process CRDT surface
/// (`enqueue_crdt_operation` → the headless executor → the Rust CRDT engine);
/// every RDF write goes through `run_sparql_update_service` into the user:rdf
/// graph (read==write). The placeholder-resolution flow: write_doc → read the
/// doc back (XML render) to recover the minted code-block id → substitute
/// `urn:wf-emit:placeholder:*` → sparql_update.
pub(crate) async fn apply_plan(
    app: &AppHandle,
    graph_id: &str,
    plan: &Plan,
    contract: &VocabularyContract,
) -> ApplyReport {
    let generator_id = format!("emporium:{}@{}", contract.name, contract.version);
    let prefix = graph_subject(graph_id);
    let user_rdf = write_graph_iri(graph_id);

    // New docs are placed at creation: look ahead for each doc's move target.
    let mut placement_for: BTreeMap<String, String> = BTreeMap::new();
    for step in &plan.steps {
        if let Step::Move { doc_id, folder_id } = step {
            placement_for
                .entry(doc_id.clone())
                .or_insert_with(|| folder_id.clone());
        }
    }
    let mut placed_at_creation: BTreeSet<String> = BTreeSet::new();

    // Captured placeholder resolutions (SCRIPT_BLOCK, TB:<docId>, …).
    let mut vars: BTreeMap<String, String> = BTreeMap::new();

    let mut steps: Vec<StepReport> = Vec::new();
    let mut script_block: Option<String> = None;

    macro_rules! halt {
        ($op:expr, $entry:expr, $error:expr) => {{
            let mut entry: serde_json::Map<String, Json> = $entry;
            entry.insert("ok".into(), json!(false));
            entry.insert(
                "error".into(),
                json!($error.chars().take(800).collect::<String>()),
            );
            steps.push(StepReport {
                op: $op.to_string(),
                extra: entry,
            });
            let halted_at = steps.len() - 1;
            log::error!(
                "emporium_apply_halt step={halted_at} op={} error={}",
                $op,
                $error.chars().take(300).collect::<String>()
            );
            return ApplyReport {
                graph: plan.graph.clone(),
                workflow: plan.workflow.clone(),
                mode: plan.mode.clone(),
                summary: serde_json::to_value(&plan.summary).unwrap_or(Json::Null),
                warnings: plan.warnings.clone(),
                steps,
                ok: false,
                halted_at: Some(halted_at),
                script_block,
                assertion: None,
            };
        }};
    }

    for step in &plan.steps {
        let op = step.op_name();
        // Base entry: the step's identifying fields (everything but bulky bodies).
        let mut entry = step_identity(step);

        match step {
            Step::CreateFolder {
                folder_id,
                label,
                parent_id,
            } => {
                // label -> `name` (gardend's payload key is `name`, NOT label).
                let payload = json!({
                    "folderId": folder_id,
                    "name": label,
                    "parentId": parent_id,
                });
                if let Err(error) =
                    enqueue(app, graph_id, "workspace.createFolder", None, payload).await
                {
                    halt!(op, entry, error);
                }
            }
            Step::RenameFolder { folder_id, label } => {
                // label -> `name`. Pass folderId as document_id AND in payload.
                let payload = json!({ "folderId": folder_id, "name": label });
                if let Err(error) = enqueue(
                    app,
                    graph_id,
                    "workspace.updateFolder",
                    Some(folder_id.clone()),
                    payload,
                )
                .await
                {
                    halt!(op, entry, error);
                }
            }
            Step::WriteDoc {
                doc_id,
                content,
                capture_script_block,
                capture_block_var,
            } => {
                // createDocument is metadata-only — content lands HERE. Place new
                // docs at creation by passing parentId (move lookahead).
                let payload = json!({
                    "documentId": doc_id,
                    "content": content,
                    "format": "markdown",
                    "parentId": placement_for.get(doc_id),
                });
                let is_new = !document_exists(app, graph_id, doc_id);
                if let Err(error) = enqueue(
                    app,
                    graph_id,
                    "document.write",
                    Some(doc_id.clone()),
                    payload,
                )
                .await
                {
                    halt!(op, entry, error);
                }
                if is_new {
                    placed_at_creation.insert(doc_id.clone());
                }

                // Read the doc back to recover the minted block ids. document.write
                // does not surface the FIRST CODE-BLOCK id directly (its blockIds
                // is every block), so we render the XML and match the code block.
                let readback_md = match read_doc_markdown(app, graph_id, doc_id) {
                    Ok(md) => md,
                    Err(error) => halt!(op, entry, error),
                };

                // Provenance: REPLACE (never accumulate) intent/render/managedBy.
                let doc_uri = format!("{prefix}:doc:{doc_id}");
                let intent = sha256_text(&canonical_md(content));
                let render = sha256_text(&canonical_md(&readback_md));
                if let Err(error) = record_doc_provenance(
                    app,
                    graph_id,
                    &user_rdf,
                    &doc_uri,
                    &intent,
                    &render,
                    &generator_id,
                )
                .await
                {
                    halt!(op, entry, error);
                }

                if capture_script_block.unwrap_or(false) || capture_block_var.is_some() {
                    let block_id = match first_code_block_id(app, graph_id, doc_id) {
                        Some(id) => id,
                        None => halt!(
                            op,
                            entry,
                            format!("no code block id in rewritten doc {doc_id}")
                        ),
                    };
                    let block_uri = format!("{prefix}:doc:{doc_id}#{block_id}");
                    if capture_script_block.unwrap_or(false) {
                        vars.insert("SCRIPT_BLOCK".to_string(), block_uri.clone());
                        script_block = Some(block_uri.clone());
                    }
                    if let Some(name) = capture_block_var {
                        vars.insert(name.clone(), block_uri);
                    }
                }
            }
            Step::Move { doc_id, folder_id } => {
                if placed_at_creation.contains(doc_id) {
                    // A move of a just-written doc is a recorded NO-OP (the write
                    // placed it via parentId at creation).
                    entry.insert("placedAtCreation".into(), json!(true));
                } else {
                    let payload = json!({
                        "documentIds": [doc_id],
                        "parentId": folder_id,
                    });
                    if let Err(error) =
                        enqueue(app, graph_id, "workspace.moveDocuments", None, payload).await
                    {
                        halt!(op, entry, error);
                    }
                }
            }
            Step::CreateWires { wires } => {
                for w in wires {
                    let WireSpec {
                        predicate,
                        source_document_id,
                        target_document_id,
                    } = w;
                    // wireId omitted: gardend mints `w-<8hex>`. Predicate stored
                    // verbatim (full URI fine); gardend creates no inverseOf mirror.
                    let payload = json!({
                        "sourceDocumentId": source_document_id,
                        "targetDocumentId": target_document_id,
                        "predicate": predicate,
                    });
                    if let Err(error) = enqueue(
                        app,
                        graph_id,
                        "workspace.createWire",
                        Some(source_document_id.clone()),
                        payload,
                    )
                    .await
                    {
                        halt!(op, entry, error);
                    }
                }
                entry.insert("count".into(), json!(wires.len()));
            }
            Step::DeleteWires { wire_ids } => {
                for wire_id in wire_ids {
                    let payload = json!({ "wireId": wire_id });
                    if let Err(error) =
                        enqueue(app, graph_id, "workspace.deleteWire", None, payload).await
                    {
                        // Tolerate "wire not found" per-wire (applier.py:251-256
                        // catches KeyError, logs, continues) so an idempotent
                        // re-apply of a delete does NOT loud-halt.
                        if error.contains("wire not found") {
                            log::warn!("emporium_wire_already_gone wire_id={wire_id}");
                        } else {
                            halt!(op, entry, error);
                        }
                    }
                }
            }
            Step::SparqlUpdate { update } => {
                let resolved = resolve_placeholders(update, &vars);
                let wrapped = match graph_wrap(&resolved, &user_rdf) {
                    Ok(w) => w,
                    Err(error) => halt!(op, entry, error),
                };
                if let Err(error) = run_update(app, graph_id, &wrapped) {
                    halt!(op, entry, error);
                }
            }
        }

        entry.insert("ok".into(), json!(true));
        steps.push(StepReport {
            op: op.to_string(),
            extra: entry,
        });
    }

    ApplyReport {
        graph: plan.graph.clone(),
        workflow: plan.workflow.clone(),
        mode: plan.mode.clone(),
        summary: serde_json::to_value(&plan.summary).unwrap_or(Json::Null),
        warnings: plan.warnings.clone(),
        steps,
        ok: true,
        halted_at: None,
        script_block,
        assertion: None,
    }
}

// ---------------------------------------------------------------------------
// CRDT surface helpers
// ---------------------------------------------------------------------------

/// Enqueue a CRDT op against the cell and await its in-process completion. The
/// headless executor drains the queue and applies through the Rust CRDT engine;
/// the returned `Err(String)` is the handler's verbatim error (e.g. the
/// `wire not found: {id}` the delete-wire handler raises).
async fn enqueue(
    app: &AppHandle,
    graph_id: &str,
    kind: &str,
    document_id: Option<String>,
    payload: Json,
) -> Result<Json, String> {
    enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: kind.to_string(),
            graph_id: graph_id.to_string(),
            document_id,
            payload,
        },
    )
    .await
}

/// Whether a document record already exists on disk (used to decide new-vs-update
/// for placement-at-creation tracking). A missing manifest reads as "new".
fn document_exists(app: &AppHandle, graph_id: &str, doc_id: &str) -> bool {
    let Ok(graph_dir) = existing_graph_dir(app, graph_id) else {
        return false;
    };
    let Ok(dir) = document_dir(&graph_dir, doc_id) else {
        return false;
    };
    dir.join("document.json").is_file()
}

/// Read a document back as canonical markdown (the render-sha provenance anchor).
/// Mirrors the spine's `document_markdown`-over-read; the in-process equivalent of
/// the Python gateway `read_document(markdown)`.
fn read_doc_markdown(app: &AppHandle, graph_id: &str, doc_id: &str) -> Result<String, String> {
    let record = read_doc_record(app, graph_id, doc_id)?;
    Ok(crate::document_export_rendering::document_markdown(&record))
}

/// Recover the first code-block id from a doc by rendering its XML
/// (`<codeBlock data-block-id="block-XXXX">…`) and matching the read-back regex.
/// `None` when the doc has no code block. Mirrors applier.py's read-back of the
/// minted block ids over the gateway (here: the in-process XML render).
fn first_code_block_id(app: &AppHandle, graph_id: &str, doc_id: &str) -> Option<String> {
    let record = read_doc_record(app, graph_id, doc_id).ok()?;
    let xml = document_xml(&record);
    code_block_id_re()
        .captures(&xml)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn read_doc_record(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
) -> Result<crate::document_service::DocumentRecord, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let dir = document_dir(&graph_dir, doc_id)?;
    let manifest = dir.join("document.json");
    read_document_record(&graph_dir, &manifest)
}

// ---------------------------------------------------------------------------
// RDF helpers (read==write graph)
// ---------------------------------------------------------------------------

/// 🔴 READ-GRAPH == WRITE-GRAPH (risk #1): the ONE place the applier resolves the
/// graph it writes RDF into. It is exactly [`user_rdf_graph_iri`] — the SAME graph
/// `survey.rs` reads — NEVER the bare `<{root}>` (banned by
/// `validate_sparql_update_authority`) and NEVER a `:projection:` graph. Every
/// `sparql_update` step and the provenance writer wrap their bodies into
/// `GRAPH <{write_graph_iri}>`. The cross-WP invariant test
/// (`fixtures::tests::cross_wp_read_eq_write_eq_user_rdf`) pins
/// `survey read graph == applier write graph == user_rdf_graph_iri` as a standing
/// regression guard.
pub(crate) fn write_graph_iri(graph_id: &str) -> String {
    user_rdf_graph_iri(graph_id)
}

/// Run a SPARQL update through the authority-validated cell service. The wrapped
/// update already names `GRAPH <{root}:user:rdf>`, which the validator passes.
fn run_update(app: &AppHandle, graph_id: &str, update: &str) -> Result<(), String> {
    run_sparql_update_service(
        app.clone(),
        SparqlUpdateInput {
            graph_id: graph_id.to_string(),
            update: update.to_string(),
        },
    )
    .map(|_| ())
    .map_err(|e: AppError| e.message())
}

/// `INSERT/DELETE DATA { body } → same verb with a GRAPH block`. Port of
/// `applier.graph_wrap`: match the leading verb, take the body between the FIRST
/// `{` and the LAST `}`, and re-emit wrapped in `GRAPH <{user_rdf}>`. The
/// planner's `render_updates` output satisfies the verb regex + trailing `}`.
fn graph_wrap(update: &str, user_rdf: &str) -> Result<String, String> {
    let verb = verb_re();
    let m = verb
        .captures(update)
        .ok_or_else(|| format!("unexpected update shape: {}", &truncate(update, 80)))?;
    if !update.trim_end().ends_with('}') {
        return Err(format!(
            "unexpected update shape: {}",
            &truncate(update, 80)
        ));
    }
    let verb_word = m.get(1).expect("verb capture present").as_str();
    let open = m.get(0).expect("match present").end();
    let close = update
        .rfind('}')
        .ok_or_else(|| format!("unexpected update shape: {}", &truncate(update, 80)))?;
    let body = &update[open..close];
    Ok(format!(
        "{verb_word} {{ GRAPH <{user_rdf}> {{\n{body}\n}} }}"
    ))
}

/// Replace every `urn:wf-emit:placeholder:NAME` with its captured concrete URI.
/// An unresolved placeholder is left verbatim (the planner never emits a TB
/// placeholder for a doc it did not also write/capture in the same plan; a
/// converged-doc TB is suppressed at plan time, see planner.rs:1454).
fn resolve_placeholders(update: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = update.to_string();
    let re = placeholder_in_update_re();
    let names: BTreeSet<String> = re
        .captures_iter(update)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect();
    for name in names {
        if let Some(value) = vars.get(&name) {
            out = out.replace(&format!("{PLACEHOLDER_NS}{name}"), value);
        }
    }
    out
}

/// Replace (never accumulate) the managed-document provenance triples in the
/// user:rdf graph. Hand-built with `GRAPH` already inline — NOT passed through
/// [`graph_wrap`]. Port of `_record_doc_provenance` (applier.py:63-84).
async fn record_doc_provenance(
    app: &AppHandle,
    graph_id: &str,
    user_rdf: &str,
    doc_uri: &str,
    intent_sha: &str,
    render_sha: &str,
    generator_id: &str,
) -> Result<(), String> {
    let mut delete = String::new();
    for p in [EMP_INTENT_SHA, EMP_RENDER_SHA, EMP_MANAGED_BY] {
        delete.push_str(&format!(
            "DELETE WHERE {{ GRAPH <{user_rdf}> {{ <{doc_uri}> <{EMPORIUM_NS}{p}> ?o }} }};\n"
        ));
    }
    let insert = format!(
        "INSERT DATA {{ GRAPH <{user_rdf}> {{ <{doc_uri}> \
         <{EMPORIUM_NS}{EMP_INTENT_SHA}> \"{intent_sha}\" ; \
         <{EMPORIUM_NS}{EMP_RENDER_SHA}> \"{render_sha}\" ; \
         <{EMPORIUM_NS}{EMP_MANAGED_BY}> \"{generator_id}\" }} }}"
    );
    run_update(app, graph_id, &format!("{delete}{insert}"))
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// The identifying fields of a step (everything but bulky bodies: content,
/// update, wires). Mirrors the Python `{k: v for k, v in step.items() if k not in
/// ("content", "update", "wires")}` projection for the report entry.
fn step_identity(step: &Step) -> serde_json::Map<String, Json> {
    let mut map = serde_json::Map::new();
    match step {
        Step::CreateFolder {
            folder_id,
            label,
            parent_id,
        } => {
            map.insert("folderId".into(), json!(folder_id));
            map.insert("label".into(), json!(label));
            map.insert("parentId".into(), json!(parent_id));
        }
        Step::RenameFolder { folder_id, label } => {
            map.insert("folderId".into(), json!(folder_id));
            map.insert("label".into(), json!(label));
        }
        Step::WriteDoc {
            doc_id,
            capture_script_block,
            capture_block_var,
            ..
        } => {
            map.insert("docId".into(), json!(doc_id));
            if let Some(c) = capture_script_block {
                map.insert("captureScriptBlock".into(), json!(c));
            }
            if let Some(v) = capture_block_var {
                map.insert("captureBlockVar".into(), json!(v));
            }
        }
        Step::Move { doc_id, folder_id } => {
            map.insert("docId".into(), json!(doc_id));
            map.insert("folderId".into(), json!(folder_id));
        }
        Step::CreateWires { .. } => {}
        Step::DeleteWires { wire_ids } => {
            map.insert("wireIds".into(), json!(wire_ids));
        }
        Step::SparqlUpdate { .. } => {}
    }
    map
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(all(test, feature = "headless"))]
mod tests;
