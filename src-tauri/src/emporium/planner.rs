//! The deterministic ingest planner — the keystone pure function.
//!
//! Port of `app/services/emporium/ingest/planner.py::plan_compute` and
//! `plan_campaign_compute`. Pure: `(parsed workflow | campaign, judgment, live
//! graph snapshot) → an ordered op list`. No I/O — the survey already did the
//! reads ([`survey`]); the content builders ([`content`]) build doc bodies; this
//! computes only the op list.
//!
//! The load-bearing guarantee is **idempotency**: the same inputs always plan
//! identically, and a converged graph plans to ZERO operations (the applier
//! re-plans before executing, which this makes free). The RDF diff is value-
//! canonical ([`terms::canon`]) so store round-trips (xsd:long → xsd:integer,
//! dateTime re-serialization) never read as drift, and minting goes strictly
//! through [`mint::render_class_triples`] which RAISES on any predicate not in
//! the frozen vocab.
//!
//! 🔴 READ-GRAPH == WRITE-GRAPH: the RDF diff is computed against survey output
//! that reads `GRAPH <{root}:user:rdf>` (`user_rdf_graph_iri`); the doc-URI
//! subject prefix is `live.prefix` (`graph_subject`). In the platform Python
//! these are the same string; in gardend they DIVERGE — the planner must use
//! `prefix` (not `read_graph`) for `doc_uri` construction, while the diff stays
//! in the user:rdf graph the survey read.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use crate::emporium::class_dispatch;
use crate::emporium::content::{
    archetype_doc_content, campaign_record_content, node_doc_content, run_doc_content,
    variant_doc_content, variant_doc_id, workflow_doc_content, VARIANT_TEXT_CAP,
};
use crate::emporium::contract::VocabularyContract;
use crate::emporium::mint::{render_class_triples, FieldValue};
use crate::emporium::schemas::{
    CampaignRecord, JudgmentInput, MemoryRecordIn, NewArchetype, ParsedWorkflow, SourceRefIn,
};
use crate::emporium::survey::Live;
use crate::emporium::terms::{
    apply_slug, canon, canonical_md, render_updates, sha256_text, Term, Triple, Value,
};
use crate::rdf::graph_subject;

/// Wire short-names we mint with custom (primary-namespace) predicate URIs; all
/// other wire predicates are builtin mnemo: terms. Mirrors `_CUSTOM_WIRE_NAMES`.
const CUSTOM_WIRE_NAMES: &[&str] = &["derivedFrom"];

/// Synthetic deterministic epoch for campaign Run triples ONLY (campaigns have
/// no wall clock, but prov:startedAtTime/endedAtTime are required on Run, and
/// determinism keeps re-ingest convergent). Mirrors `CAMPAIGN_EPOCH_MS`.
const CAMPAIGN_EPOCH_MS: i64 = 1_780_900_000_000;

/// The size argument `render_updates` chunks on (matches `terms.render_updates`'s
/// default of 60).
const UPDATE_CHUNK: usize = 60;

/// The frozen memory pack identity. The content-hash subject input set is frozen
/// behind this version: changing the hashed field set (in [`memory_record_id`])
/// re-mints every memory subject, so it MUST bump this version (the standing
/// content-addressed-key-fragility risk, §10). Stamped nowhere on the record in
/// v1 (it is the planner's identity contract, not authoritative data).
pub(crate) const MEMORY_PACK_VERSION: &str = "sophia-memory-core@1.1.0";

/// The unit separator used to join the content-hash input fields — a byte that
/// cannot appear in the inputs, so distinct field tuples cannot collide.
const HASH_SEP: char = '\u{1f}';

/// A planner error — mirrors the Python `PlanError` surfaced from the planner.
pub(crate) use crate::emporium::terms::PlanError;

/// A current wire as the survey hands it to the planner. Mirrors the Python wire
/// dicts (`{id, predicate, sourceDocumentId, targetDocumentId}`). The survey
/// (HELD wire read) will produce these; the tests construct them directly.
#[derive(Debug, Clone, Default)]
pub(crate) struct CurrentWire {
    pub(crate) id: String,
    pub(crate) predicate: String,
    pub(crate) source_document_id: String,
    pub(crate) target_document_id: String,
}

/// One planned op. JSON-serializable; the variants are the step grammar
/// (`create_folder | rename_folder | write_doc | move | create_wires |
/// delete_wires | sparql_update`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Step {
    CreateFolder {
        #[serde(rename = "folderId")]
        folder_id: String,
        label: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
    },
    RenameFolder {
        #[serde(rename = "folderId")]
        folder_id: String,
        label: String,
    },
    WriteDoc {
        #[serde(rename = "docId")]
        doc_id: String,
        content: String,
        #[serde(rename = "captureScriptBlock", skip_serializing_if = "Option::is_none")]
        capture_script_block: Option<bool>,
        #[serde(rename = "captureBlockVar", skip_serializing_if = "Option::is_none")]
        capture_block_var: Option<String>,
    },
    Move {
        #[serde(rename = "docId")]
        doc_id: String,
        #[serde(rename = "folderId")]
        folder_id: String,
    },
    CreateWires {
        wires: Vec<WireSpec>,
    },
    DeleteWires {
        #[serde(rename = "wireIds")]
        wire_ids: Vec<String>,
    },
    SparqlUpdate {
        update: String,
    },
}

impl Step {
    pub(crate) fn op_name(&self) -> &'static str {
        match self {
            Step::CreateFolder { .. } => "create_folder",
            Step::RenameFolder { .. } => "rename_folder",
            Step::WriteDoc { .. } => "write_doc",
            Step::Move { .. } => "move",
            Step::CreateWires { .. } => "create_wires",
            Step::DeleteWires { .. } => "delete_wires",
            Step::SparqlUpdate { .. } => "sparql_update",
        }
    }
}

/// A wire to create: `{predicate, source_document_id, target_document_id}`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct WireSpec {
    pub(crate) predicate: String,
    pub(crate) source_document_id: String,
    pub(crate) target_document_id: String,
}

/// The plan summary counts (mirrors the Python `plan["summary"]`).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PlanSummary {
    pub(crate) folders: usize,
    #[serde(rename = "docWrites")]
    pub(crate) doc_writes: usize,
    pub(crate) moves: usize,
    #[serde(rename = "wiresCreate")]
    pub(crate) wires_create: usize,
    #[serde(rename = "wiresDelete")]
    pub(crate) wires_delete: usize,
    #[serde(rename = "rdfDelete")]
    pub(crate) rdf_delete: usize,
    #[serde(rename = "rdfInsert")]
    pub(crate) rdf_insert: usize,
}

/// The full plan: ordered ops + summary + warnings. Mirrors the Python plan dict.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Plan {
    pub(crate) graph: String,
    pub(crate) workflow: String,
    /// The contract name this plan was minted against (`workflow` | `campaign`
    /// uses the wf contract's name `workflow`; memory uses `sophia-memory-core`).
    /// NOTE: this is the pack NAME — do NOT route the apply-dispatch fork on it
    /// (`"sophia-memory-core"` does not start with "mem"). Use
    /// [`Plan::routes_to_memory_sink`] (which keys on `mode`) instead.
    pub(crate) vocab: String,
    /// The plan's archetype mode: a wf rendering mode, `"campaign"`, or `"memory"`.
    /// This is the apply-dispatch signal (see [`Plan::routes_to_memory_sink`]).
    pub(crate) mode: String,
    #[serde(rename = "shortId")]
    pub(crate) short_id: String,
    #[serde(rename = "workflowDocId")]
    pub(crate) workflow_doc_id: String,
    pub(crate) steps: Vec<Step>,
    pub(crate) summary: PlanSummary,
    pub(crate) warnings: Vec<String>,
    /// The DESIRED INSERT triples this plan will write (EA-6) — the exact typed
    /// terms the planner minted for the `INSERT DATA` chunks, retained so the
    /// memory apply fork can SHACL-validate them WITHOUT re-parsing N-Triples out
    /// of the rendered SPARQL (the structurally-faithful path: the validator sees
    /// the same `Triple`s the renderer serialized). DELETE/demote triples are NOT
    /// captured — they reference already-valid live state and are not validated.
    /// Empty for the wf/campaign appliers (they validate opt-in at reconcile, and
    /// `validate_desired([])` conforms trivially). Skipped in serialization — it
    /// is an in-process apply detail, not part of the JSON plan contract.
    #[serde(skip)]
    pub(crate) desired_inserts: Vec<Triple>,
    /// The OBSERVER (witness) this memory plan is attributed to (Variant B). Empty
    /// for the shared commons and for every non-memory plan. The apply fork reads
    /// this to wrap the rendered updates into the SAME per-observer projection
    /// graph the planner minted the subjects under (they MUST agree). Skipped in
    /// serialization when empty so non-memory plan JSON is unchanged.
    #[serde(
        rename = "observerAgentId",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub(crate) observer: String,
    /// The wall-clock ms `plan_memory_compute_at` stamped this plan with (0 for
    /// every non-memory plan, skipped in serialization). Recorded so the memory
    /// EVENT LOG can capture the exact clock the plan used — replaying the
    /// event through the planner with the same clock reproduces the plan (and
    /// therefore the projection) byte-for-byte.
    #[serde(rename = "plannedAtMs", default, skip_serializing_if = "ms_is_zero")]
    pub(crate) planned_at_ms: i64,
}

fn ms_is_zero(v: &i64) -> bool {
    *v == 0
}

impl Plan {
    /// The per-observer projection graph this plan's updates must be wrapped into
    /// (the apply fork's single source of truth — see [`plan_memory_compute`]).
    pub(crate) fn memory_graph_iri(&self, graph_id: &str) -> String {
        crate::rdf_authority::memory_projection_graph_iri_for(graph_id, &self.observer)
    }

    /// Whether the apply-dispatch fork (spine `apply_and_assert`) must route this
    /// plan to the direct-on-store memory materializer (`:projection:memory`)
    /// rather than the user:rdf applier. Keyed on `mode == DispatchRoute::
    /// MemorySink.mode_label()` (set by [`plan_memory_compute_at`] from the
    /// memory contract's OWN declared signature, T5.22 — not a bare hardcoded
    /// literal) — NOT on `vocab`, whose memory value is the pack name
    /// `"sophia-memory-core"`, which does not start with "mem". Routing on vocab
    /// silently leaked typed memory into `:user:rdf`.
    pub(crate) fn routes_to_memory_sink(&self) -> bool {
        self.mode == class_dispatch::DispatchRoute::MemorySink.mode_label()
    }

    /// Whether the apply-dispatch fork must route this plan to the GENERIC
    /// simple-projection applier (EA-3 / B4) — reconcile the plan's
    /// `desired_inserts` into the vocab's `write_target` projection sink. Keyed
    /// on `mode == DispatchRoute::SimpleProjection.mode_label()` (set by
    /// [`plan_generic_compute`]). Like the memory fork it keys on MODE, not
    /// `vocab`, so a new projection vocab needs no dispatch edit (the
    /// table-driven registry + the mode is the whole wiring).
    pub(crate) fn routes_to_simple_projection(&self) -> bool {
        self.mode == class_dispatch::DispatchRoute::SimpleProjection.mode_label()
    }
}

// ---------------------------------------------------------------------------
// Small contract/term helpers (ported from the module-level helpers in planner.py)
// ---------------------------------------------------------------------------

fn ns<'a>(contract: &'a VocabularyContract, prefix: &str) -> &'a str {
    contract
        .namespaces
        .get(prefix)
        .map(String::as_str)
        .unwrap_or_default()
}

/// Wire APIs shorten predicates for display — restore the full URI. Mirrors
/// `planner.norm_pred`.
fn norm_pred(contract: &VocabularyContract, pred: &str) -> String {
    if !pred.is_empty() && !pred.contains('#') && !pred.contains('/') {
        let namespace = if CUSTOM_WIRE_NAMES.contains(&pred) {
            contract.primary_namespace()
        } else {
            ns(contract, "mnemo")
        };
        format!("{namespace}{pred}")
    } else {
        pred.to_string()
    }
}

fn slug(contract: &VocabularyContract, text: &str) -> String {
    match &contract.slug_rule {
        Some(rule) => apply_slug(rule, text),
        None => text.to_string(),
    }
}

/// Recorded-provenance convergence (no round-trip equivalence inference).
/// Mirrors `planner._doc_converged`. Pushes a warning when the live doc was
/// hand-edited since our last write.
fn doc_converged(
    did: &str,
    content: &str,
    docs_md: &BTreeMap<String, String>,
    provenance: &BTreeMap<String, BTreeMap<String, String>>,
    warnings: &mut Vec<String>,
) -> bool {
    let Some(cur) = docs_md.get(did) else {
        return false;
    };
    let empty = BTreeMap::new();
    let prov = provenance.get(did).unwrap_or(&empty);
    let render_sha = prov.get("renderSha256");
    let intent_sha = prov.get("intentSha256");
    let (Some(render_sha), Some(intent_sha)) = (render_sha, intent_sha) else {
        return false;
    };
    if render_sha.is_empty() || intent_sha.is_empty() {
        return false;
    }
    if sha256_text(&canonical_md(cur)) != *render_sha {
        warnings.push(format!(
            "doc {did} hand-edited since last ingest; will be overwritten"
        ));
        return false;
    }
    *intent_sha == sha256_text(&canonical_md(content))
}

/// Validate a judgment against the live graph. Mirrors `planner.validate_judgment`.
/// Returns `(node_arch: {label -> archetypeDocId}, new_arch_docs)`.
fn validate_judgment(
    contract: &VocabularyContract,
    judgment: &JudgmentInput,
    live: &Live,
) -> Result<(BTreeMap<String, String>, Vec<NewArchDoc>), PlanError> {
    let mut node_arch: BTreeMap<String, String> = BTreeMap::new();
    let mut new_arch_docs: Vec<NewArchDoc> = Vec::new();
    let by_slug: BTreeMap<&str, &NewArchetype> = judgment
        .new_archetypes
        .iter()
        .map(|a| (a.slug.as_str(), a))
        .collect();
    let known_arch: BTreeSet<&str> = live
        .archetypes
        .values()
        .filter_map(|a| a.doc_id.as_deref())
        .collect();

    for (label, target) in &judgment.node_archetypes {
        if !target.starts_with("NEW:") && !known_arch.contains(target.as_str()) {
            return Err(PlanError(format!(
                "judgment assigns unknown archetype '{target}' to node '{label}' — \
                 not in graph and not NEW: (judgments are untrusted input)"
            )));
        }
        if let Some(rest) = target.strip_prefix("NEW:") {
            let Some(a) = by_slug.get(rest) else {
                return Err(PlanError(format!(
                    "judgment references {target} but newArchetypes has no slug {rest}"
                )));
            };
            let did = format!("agent-{}", slug(contract, &a.slug));
            node_arch.insert(label.clone(), did.clone());
            if !new_arch_docs.iter().any(|d| d.doc_id == did) {
                new_arch_docs.push(NewArchDoc {
                    arch: (*a).clone(),
                    doc_id: did,
                });
            }
        } else {
            node_arch.insert(label.clone(), target.clone());
        }
    }
    Ok((node_arch, new_arch_docs))
}

/// A NEW: archetype to mint — the source `NewArchetype` plus its resolved doc id.
#[derive(Debug, Clone)]
struct NewArchDoc {
    arch: NewArchetype,
    doc_id: String,
}

// ---------------------------------------------------------------------------
// FieldValue construction helpers — typed coercion of Option<…> into the mint's
// optional-field map. An absent field is simply not inserted (the mint treats
// a missing key as "not provided"; required → PlanError).
// ---------------------------------------------------------------------------

fn put(map: &mut BTreeMap<String, FieldValue>, key: &str, value: Value) {
    map.insert(key.to_string(), FieldValue::One(value));
}

fn put_opt(map: &mut BTreeMap<String, FieldValue>, key: &str, value: Option<Value>) {
    if let Some(v) = value {
        map.insert(key.to_string(), FieldValue::One(v));
    }
}

fn put_many(map: &mut BTreeMap<String, FieldValue>, key: &str, values: Vec<Value>) {
    if !values.is_empty() {
        map.insert(key.to_string(), FieldValue::Many(values));
    }
}

// ---------------------------------------------------------------------------
// plan_compute — the workflow/run path.
// ---------------------------------------------------------------------------

/// Pure: `(parsed, judgment, live snapshot) → ordered op list`. No I/O.
///
/// `parsed` is the typed workflow; `parsed_json` and `run_json` are the same
/// payload as loosely-typed JSON for the content builders (which reproduce
/// Python dict access + `json.dumps` byte-for-byte). `run_json` is the `run`
/// sub-object (or `None` for a script-only def).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_compute(
    contract: &VocabularyContract,
    parsed: &ParsedWorkflow,
    parsed_json: &Json,
    run_json: Option<&Json>,
    judgment: Option<&JudgmentInput>,
    live: &Live,
    current_triples: &[Triple],
    current_wires: &[CurrentWire],
    current_docs_md: &BTreeMap<String, String>,
    doc_provenance: &BTreeMap<String, BTreeMap<String, String>>,
    journal_doc_uri: Option<&str>,
) -> Result<Plan, PlanError> {
    let wfns = contract.primary_namespace().to_string();
    let mnemo = ns(contract, "mnemo").to_string();
    let prov = ns(contract, "prov").to_string();
    let dcterms = ns(contract, "dcterms").to_string();
    let rdf_type = format!("{}type", ns(contract, "rdf"));

    let name = parsed.name.clone();
    let existing = live.workflows.get(&name).cloned();
    let mut warnings: Vec<String> = Vec::new();
    let mut steps: Vec<Step> = Vec::new();

    let prefix = live.prefix.clone();
    if prefix.is_empty() {
        return Err(PlanError(
            "live snapshot carries no graph URI prefix (survey bug)".to_string(),
        ));
    }

    // ── mode & identity ──
    let sha_matches = existing
        .as_ref()
        .map(|e| e.sha.as_deref() == Some(parsed.script_sha256.as_str()))
        .unwrap_or(false);
    let runs_only = existing
        .as_ref()
        .map(|e| {
            parsed.kind == "run-record"
                && !sha_matches
                && e.sha.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
        })
        .unwrap_or(false);
    if runs_only {
        let stored = existing
            .as_ref()
            .and_then(|e| e.sha.as_deref())
            .unwrap_or("");
        warnings.push(format!(
            "run record's script sha {} != stored {} — filing run only; \
             structure/doc untouched (re-ingest the current script separately)",
            short_sha(&parsed.script_sha256),
            short_sha(stored),
        ));
    }

    let mut wf_doc_id: String;
    let mut wf_folder: Option<String>;
    let short: String;
    let mut existing_nodes: BTreeMap<String, String> = BTreeMap::new();
    if let Some(ex) = &existing {
        wf_doc_id = ex.doc_id.clone().unwrap_or_default();
        wf_folder = live.docs.get(&wf_doc_id).and_then(|d| d.folder_id.clone());
        short = match &wf_folder {
            Some(f) => f.strip_suffix("-folder").unwrap_or(f).to_string(),
            None => wf_doc_id.clone(),
        };
        if let Some(nodes) = live.nodes_by_workflow.get(&ex.uri) {
            existing_nodes = nodes.clone();
        }
    } else {
        let Some(s) = judgment
            .and_then(|j| j.short_id.clone())
            .filter(|s| !s.is_empty())
        else {
            return Err(PlanError(
                "new workflow: judgment.shortId required (run judge-prompt)".to_string(),
            ));
        };
        short = s;
        wf_doc_id = short.clone();
        wf_folder = Some(format!("{short}-folder"));
        let folder_key = format!("{short}-folder");
        if live.docs.contains_key(&short) || live.folders.contains_key(&folder_key) {
            // docs/folders exist but no wf:Workflow in RDF: a prior apply halted
            // before the RDF stage — resume over the same ids rather than refusing.
            warnings.push(format!(
                "resuming partial prior apply over existing '{short}' structure \
                 (no wf:Workflow in RDF)"
            ));
            if let Some(f) = live.docs.get(&short).and_then(|d| d.folder_id.clone()) {
                wf_folder = Some(f);
            }
            for n in &parsed.nodes {
                let nid = format!("{short}-n-{}", slug(contract, &n.label));
                if live.docs.contains_key(&nid) {
                    existing_nodes.insert(n.label.clone(), nid);
                }
            }
        }
    }

    let doc_uri = |d: &str| format!("{prefix}:doc:{d}");
    let wf_uri = doc_uri(&wf_doc_id);

    // ── archetype resolution ──
    let mut node_arch: BTreeMap<String, String> = BTreeMap::new();
    let mut new_arch_docs: Vec<NewArchDoc> = Vec::new();
    if let Some(j) = judgment {
        let (na, nad) = validate_judgment(contract, j, live)?;
        node_arch = na;
        new_arch_docs = nad;
    }
    // inherit archetypes from existing exemplifies wires (judgment-free path).
    for w in current_wires {
        if norm_pred(contract, &w.predicate).ends_with("#exemplifies") {
            let (src, tgt) = (&w.source_document_id, &w.target_document_id);
            for (label, did) in &existing_nodes {
                if did == src && !node_arch.contains_key(label) {
                    node_arch.insert(label.clone(), tgt.clone());
                }
            }
        }
    }

    // ── folders ──
    let mut planned_folders: BTreeSet<String> = BTreeSet::new();

    // ensure_folder appends a create/rename step the first time a folder id is
    // touched (and only if it is missing or mislabelled). Mirrors the closure.
    let mut ensure_folder = |fid: &str,
                             label: &str,
                             parent: Option<&str>,
                             steps: &mut Vec<Step>,
                             planned: &mut BTreeSet<String>| {
        if planned.contains(fid) {
            return;
        }
        match live.folders.get(fid) {
            None => {
                planned.insert(fid.to_string());
                steps.push(Step::CreateFolder {
                    folder_id: fid.to_string(),
                    label: label.to_string(),
                    parent_id: parent.map(str::to_string),
                });
            }
            Some(cur) if cur.label != label => {
                planned.insert(fid.to_string());
                steps.push(Step::RenameFolder {
                    folder_id: fid.to_string(),
                    label: label.to_string(),
                });
            }
            Some(_) => {}
        }
    };

    let mut node_folder: BTreeMap<String, String> = BTreeMap::new();
    let mut phase_folder_of: BTreeMap<i64, String> = BTreeMap::new();
    if !runs_only {
        let mut root = live
            .folders
            .iter()
            .find(|(_, f)| f.label == "Workflows" && f.parent_id.is_none())
            .map(|(fid, _)| fid.clone());
        if root.is_none() {
            let rid = contract
                .folders
                .get("workflowsRoot")
                .map(|f| f.folder_id.clone())
                .unwrap_or_else(|| "workflows".to_string());
            ensure_folder(&rid, "Workflows", None, &mut steps, &mut planned_folders);
            root = Some(rid);
        }
        let root = root.unwrap_or_default();
        let wff = wf_folder
            .clone()
            .unwrap_or_else(|| format!("{short}-folder"));
        wf_folder = Some(wff.clone());
        ensure_folder(&wff, &name, Some(&root), &mut steps, &mut planned_folders);
        for ph in &parsed.phases {
            let want_label = format!("{} · {}", ph.order, ph.title);
            // match an existing phase folder under wf_folder by "{order} ·" prefix.
            let order_prefix = format!("^{}\\s*·", ph.order);
            let re = regex::Regex::new(&order_prefix).ok();
            let fid = live
                .folders
                .iter()
                .find(|(_, v)| {
                    v.parent_id.as_deref() == Some(wff.as_str())
                        && re.as_ref().map(|r| r.is_match(&v.label)).unwrap_or(false)
                })
                .map(|(f, _)| f.clone())
                .unwrap_or_else(|| format!("{short}-p{}", ph.order));
            ensure_folder(
                &fid,
                &want_label,
                Some(&wff),
                &mut steps,
                &mut planned_folders,
            );
            phase_folder_of.insert(ph.order, fid);
        }
        for n in &parsed.nodes {
            let f = phase_folder_of
                .get(&n.phase_index)
                .cloned()
                .unwrap_or_else(|| wff.clone());
            node_folder.insert(n.label.clone(), f);
        }
    }
    let mut prov_folder: Option<String> = None;
    if run_json.is_some() {
        let wff = wf_folder
            .clone()
            .unwrap_or_else(|| format!("{short}-folder"));
        let found = live
            .folders
            .iter()
            .find(|(_, v)| v.parent_id.as_deref() == Some(wff.as_str()) && v.label == "Provenance")
            .map(|(f, _)| f.clone());
        prov_folder = Some(match found {
            Some(f) => f,
            None => {
                let pf = format!("{short}-prov");
                ensure_folder(
                    &pf,
                    "Provenance",
                    Some(&wff),
                    &mut steps,
                    &mut planned_folders,
                );
                pf
            }
        });
    }

    // ── documents ──
    // (docId, content, folder, captureScriptBlock)
    let mut docs_to_write: Vec<(String, String, Option<String>, bool)> = Vec::new();
    let mut node_doc_id: BTreeMap<String, String> = BTreeMap::new();
    if !runs_only {
        // without judgment, an existing doc with a matching script is
        // authoritative (its preamble was judgment-authored; don't clobber it).
        if judgment.is_some() || existing.is_none() || !sha_matches {
            let preamble = judgment.map(|j| j.preamble.as_str());
            docs_to_write.push((
                wf_doc_id.clone(),
                workflow_doc_content(parsed, preamble),
                wf_folder.clone(),
                true,
            ));
        }
        let phase_titles: BTreeMap<i64, &str> = parsed
            .phases
            .iter()
            .map(|p| (p.order, p.title.as_str()))
            .collect();
        for n in &parsed.nodes {
            let did = existing_nodes
                .get(&n.label)
                .cloned()
                .unwrap_or_else(|| format!("{short}-n-{}", slug(contract, &n.label)));
            node_doc_id.insert(n.label.clone(), did.clone());
            let phase_title = phase_titles.get(&n.phase_index).copied().unwrap_or("");
            docs_to_write.push((
                did,
                node_doc_content(n, phase_title),
                node_folder.get(&n.label).cloned(),
                false,
            ));
        }
        for a in &new_arch_docs {
            let mut lib = live
                .folders
                .iter()
                .find(|(_, f)| f.label == "Agent Library" && f.parent_id.is_none())
                .map(|(fid, _)| fid.clone());
            if lib.is_none() {
                let lid = contract
                    .folders
                    .get("agentLibrary")
                    .map(|f| f.folder_id.clone())
                    .unwrap_or_else(|| "agent-library".to_string());
                ensure_folder(
                    &lid,
                    "Agent Library",
                    None,
                    &mut steps,
                    &mut planned_folders,
                );
                lib = Some(lid);
            }
            docs_to_write.push((a.doc_id.clone(), archetype_doc_content(&a.arch), lib, false));
        }
    } else {
        node_doc_id = existing_nodes.clone();
    }

    let mut run_doc_id: Option<String> = None;
    if let Some(run) = run_json {
        let run_id = run.get("runId").and_then(Json::as_str).unwrap_or("");
        let rdid = format!("{short}-run-{}", slug(contract, run_id));
        docs_to_write.push((
            rdid.clone(),
            run_doc_content(parsed_json, run),
            prov_folder.clone(),
            false,
        ));
        run_doc_id = Some(rdid);
    }

    let mut wrote_script_doc = false;
    for (did, content, folder, capture) in &docs_to_write {
        if doc_converged(did, content, current_docs_md, doc_provenance, &mut warnings) {
            // converged — skip rewrite.
        } else {
            steps.push(Step::WriteDoc {
                doc_id: did.clone(),
                content: content.clone(),
                capture_script_block: if *capture { Some(true) } else { None },
                capture_block_var: None,
            });
            if *capture {
                wrote_script_doc = true;
            }
        }
        let cur_folder = live.docs.get(did).and_then(|d| d.folder_id.clone());
        if let Some(f) = folder {
            if cur_folder.as_deref() != Some(f.as_str()) {
                steps.push(Step::Move {
                    doc_id: did.clone(),
                    folder_id: f.clone(),
                });
            }
        }
    }

    // ── wires ──
    let mut desired_wires: BTreeSet<(String, String, String)> = BTreeSet::new();
    if !runs_only {
        for edge in &parsed.edges {
            if edge.len() == 2 {
                let (a, b) = (&edge[0], &edge[1]);
                if let (Some(sa), Some(sb)) = (node_doc_id.get(a), node_doc_id.get(b)) {
                    desired_wires.insert((format!("{mnemo}flowsInto"), sa.clone(), sb.clone()));
                }
            }
        }
        for (label, arch_did) in &node_arch {
            if let Some(sd) = node_doc_id.get(label) {
                desired_wires.insert((format!("{mnemo}exemplifies"), sd.clone(), arch_did.clone()));
            }
        }
    }
    let mut have: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for w in current_wires {
        let key = (
            norm_pred(contract, &w.predicate),
            w.source_document_id.clone(),
            w.target_document_id.clone(),
        );
        have.entry(key).or_default().push(w.id.clone());
    }
    // to_create: sorted desired keys not present (BTreeSet iterates sorted).
    let to_create: Vec<(String, String, String)> = desired_wires
        .iter()
        .filter(|k| !have.contains_key(*k))
        .cloned()
        .collect();
    let node_doc_id_values: BTreeSet<&str> = node_doc_id.values().map(String::as_str).collect();
    let mut to_delete: Vec<String> = Vec::new();
    for (key, ids) in &have {
        if ids.len() > 1 {
            to_delete.extend_from_slice(&ids[1..]);
        }
        let (pred, src, _tgt) = key;
        if pred.ends_with("#exemplifies")
            && node_doc_id_values.contains(src.as_str())
            && !desired_wires.contains(key)
            && !runs_only
            && judgment.is_some()
        {
            to_delete.push(ids[0].clone());
        }
    }
    if !to_create.is_empty() {
        steps.push(Step::CreateWires {
            wires: to_create
                .iter()
                .map(|(p, s, t)| WireSpec {
                    predicate: p.clone(),
                    source_document_id: s.clone(),
                    target_document_id: t.clone(),
                })
                .collect(),
        });
    }
    if !to_delete.is_empty() {
        let sorted: BTreeSet<String> = to_delete.iter().cloned().collect();
        steps.push(Step::DeleteWires {
            wire_ids: sorted.into_iter().collect(),
        });
    }

    // ── RDF ──
    let mut desired: Vec<Triple> = Vec::new();
    if !runs_only {
        let phase_uri =
            |order: i64| format!("urn:sophia:wf:{}:phase:{order}", slug(contract, &name));
        // wf:scriptBlock: a placeholder unless the script doc was NOT rewritten,
        // in which case reuse the concrete value already in the graph.
        let mut script_block = Value::Placeholder("SCRIPT_BLOCK".to_string());
        if !wrote_script_doc {
            let sb = current_triples.iter().find_map(|(s, p, o)| {
                if s == &wf_uri && p == &format!("{wfns}scriptBlock") {
                    term_object_value(o)
                } else {
                    None
                }
            });
            if let Some(v) = sb {
                script_block = v;
            }
        }
        let mut wf_values: BTreeMap<String, FieldValue> = BTreeMap::new();
        put(&mut wf_values, "wf:name", Value::Str(name.clone()));
        put(
            &mut wf_values,
            "wf:description",
            Value::Str(parsed.description.clone()),
        );
        put_opt(
            &mut wf_values,
            "wf:whenToUse",
            parsed.when_to_use.clone().map(Value::Str),
        );
        put(
            &mut wf_values,
            "wf:scriptSha256",
            Value::Str(parsed.script_sha256.clone()),
        );
        put(&mut wf_values, "wf:scriptBlock", script_block);
        put_many(
            &mut wf_values,
            "wf:phase",
            parsed
                .phases
                .iter()
                .map(|p| Value::Uri(phase_uri(p.order)))
                .collect(),
        );
        desired.extend(render_class_triples(
            contract, &wf_uri, "Workflow", &wf_values,
        )?);

        for p in &parsed.phases {
            let mut pv: BTreeMap<String, FieldValue> = BTreeMap::new();
            put(&mut pv, "wf:order", Value::Int(p.order));
            put(&mut pv, "dcterms:title", Value::Str(p.title.clone()));
            put_opt(
                &mut pv,
                "dcterms:description",
                p.detail.clone().map(Value::Str),
            );
            desired.extend(render_class_triples(
                contract,
                &phase_uri(p.order),
                "Phase",
                &pv,
            )?);
        }
        for n in &parsed.nodes {
            let did = &node_doc_id[&n.label];
            let mut nv: BTreeMap<String, FieldValue> = BTreeMap::new();
            put(&mut nv, "wf:label", Value::Str(n.label.clone()));
            put(&mut nv, "wf:phaseIndex", Value::Int(n.phase_index));
            put(&mut nv, "wf:partOfWorkflow", Value::Uri(wf_uri.clone()));
            put_opt(
                &mut nv,
                "wf:agentType",
                n.agent_type.clone().map(Value::Str),
            );
            desired.extend(render_class_triples(
                contract,
                &doc_uri(did),
                "AgentNode",
                &nv,
            )?);
        }
        for a in &new_arch_docs {
            let mut av: BTreeMap<String, FieldValue> = BTreeMap::new();
            put(&mut av, "wf:name", Value::Str(a.arch.slug.clone()));
            desired.extend(render_class_triples(
                contract,
                &doc_uri(&a.doc_id),
                "Archetype",
                &av,
            )?);
        }
    }

    if let Some(run) = run_json {
        let run_id = run
            .get("runId")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string();
        let run_uri = format!("urn:sophia:wf-run:{run_id}");
        // End time fallback chain: ms epoch → ISO string → start+duration.
        let ended: Option<Value> = run
            .get("endTimeMs")
            .and_then(Json::as_i64)
            .map(Value::Int)
            .or_else(|| {
                run.get("endTimeIso")
                    .and_then(Json::as_str)
                    .map(|s| Value::Str(s.to_string()))
            })
            .or_else(|| {
                let start = run.get("startTimeMs").and_then(Json::as_i64);
                let dur = run
                    .get("durationMs")
                    .and_then(Json::as_i64)
                    .filter(|d| *d != 0);
                match (start, dur) {
                    (Some(s), Some(d)) => Some(Value::Int(s + d)),
                    _ => None,
                }
            });
        // journal: param → current_triples wf:journalDocument.
        let journal: Option<Value> =
            journal_doc_uri
                .map(|j| Value::Uri(j.to_string()))
                .or_else(|| {
                    current_triples.iter().find_map(|(s, p, o)| {
                        if s == &run_uri && p == &format!("{wfns}journalDocument") {
                            term_object_value(o)
                        } else {
                            None
                        }
                    })
                });
        let generated_files: Vec<Value> = current_triples
            .iter()
            .filter(|(s, p, _)| s == &run_uri && p == &format!("{wfns}generatedFile"))
            .filter_map(|(_, _, o)| term_object_value(o))
            .collect();
        let prov_generated: Vec<Value> = current_triples
            .iter()
            .filter(|(s, p, _)| s == &run_uri && p == &format!("{prov}generated"))
            .filter_map(|(_, _, o)| term_object_value(o))
            .collect();

        let mut rv: BTreeMap<String, FieldValue> = BTreeMap::new();
        put(&mut rv, "wf:runId", Value::Str(run_id.clone()));
        put(&mut rv, "wf:workflowName", Value::Str(name.clone()));
        put(&mut rv, "wf:status", jstr_val(run, "status"));
        put(
            &mut rv,
            "wf:totalTokens",
            Value::Int(jint(run, "totalTokens")),
        );
        put(
            &mut rv,
            "wf:agentCount",
            Value::Int(jint(run, "agentCount")),
        );
        put(
            &mut rv,
            "wf:durationMs",
            Value::Int(jint(run, "durationMs")),
        );
        put(&mut rv, "prov:used", Value::Uri(wf_uri.clone()));
        put_opt(
            &mut rv,
            "prov:startedAtTime",
            run.get("startTimeMs")
                .and_then(Json::as_i64)
                .map(Value::Int),
        );
        put_opt(&mut rv, "prov:endedAtTime", ended);
        if let Some(rdid) = &run_doc_id {
            put(&mut rv, "wf:runRecordDocument", Value::Uri(doc_uri(rdid)));
        }
        put_opt(&mut rv, "wf:journalDocument", journal);
        put_opt(
            &mut rv,
            "wf:sessionRecord",
            run.get("recordPath")
                .and_then(Json::as_str)
                .map(|s| Value::Str(s.to_string())),
        );
        put_many(&mut rv, "wf:generatedFile", generated_files);
        put_many(&mut rv, "prov:generated", prov_generated);
        put_opt(
            &mut rv,
            "wf:scriptSource",
            if runs_only {
                Some(Value::Str(format!("sha256:{}", parsed.script_sha256)))
            } else {
                None
            },
        );
        desired.extend(render_class_triples(contract, &run_uri, "Run", &rv)?);

        let mut unmatched: Vec<String> = Vec::new();
        if let Some(agents) = run.get("agents").and_then(Json::as_array) {
            for a in agents {
                let label = a
                    .get("label")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string();
                let node_did = node_doc_id.get(&label).cloned();
                if node_did.is_none() {
                    unmatched.push(label.clone());
                }
                let agent_uri = format!("{run_uri}:agent:{}", slug(contract, &label));
                let mut arv: BTreeMap<String, FieldValue> = BTreeMap::new();
                put(&mut arv, "wf:partOfRun", Value::Uri(run_uri.clone()));
                put(&mut arv, "wf:label", Value::Str(label.clone()));
                put(&mut arv, "wf:phaseIndex", Value::Int(jint(a, "phaseIndex")));
                put(&mut arv, "wf:model", jstr_val(a, "model"));
                put(&mut arv, "wf:state", jstr_val(a, "state"));
                put(&mut arv, "wf:tokens", Value::Int(jint(a, "tokens")));
                put(&mut arv, "wf:toolCalls", Value::Int(jint(a, "toolCalls")));
                put(&mut arv, "wf:durationMs", Value::Int(jint(a, "durationMs")));
                put_opt(
                    &mut arv,
                    "wf:queuedAt",
                    a.get("queuedAt").and_then(Json::as_i64).map(Value::Int),
                );
                put_opt(
                    &mut arv,
                    "wf:startedAt",
                    a.get("startedAt").and_then(Json::as_i64).map(Value::Int),
                );
                put(
                    &mut arv,
                    "wf:cached",
                    Value::Bool(a.get("cached").and_then(Json::as_bool).unwrap_or(false)),
                );
                put_opt(
                    &mut arv,
                    "wf:node",
                    node_did.as_ref().map(|d| Value::Uri(doc_uri(d))),
                );
                put_opt(
                    &mut arv,
                    "wf:archetype",
                    node_arch.get(&label).map(|d| Value::Uri(doc_uri(d))),
                );
                put_opt(
                    &mut arv,
                    "wf:agentType",
                    a.get("agentType")
                        .and_then(Json::as_str)
                        .map(|s| Value::Str(s.to_string())),
                );
                desired.extend(render_class_triples(
                    contract, &agent_uri, "AgentRun", &arv,
                )?);
            }
        }
        if !unmatched.is_empty() {
            warnings.push(format!(
                "run agents with no matching node (no wf:node link): {unmatched:?}"
            ));
        }
    }

    // ── diff RDF against live ──
    let managed_subjects: BTreeSet<&str> = desired.iter().map(|(s, _, _)| s.as_str()).collect();
    let cur: Vec<&Triple> = current_triples
        .iter()
        .filter(|(s, _, _)| managed_subjects.contains(s.as_str()))
        .collect();
    let desired_canon: BTreeSet<_> = desired.iter().map(canon).collect();
    let cur_canon: BTreeSet<_> = cur.iter().map(|t| canon(t)).collect();

    let bound_to_agent = format!("{wfns}boundToAgent");
    let is_managed_pred = |p: &str, o: &Term| -> bool {
        if p == bound_to_agent {
            return false;
        }
        if p.starts_with(&wfns) || p.starts_with(&prov) {
            return true;
        }
        if p == rdf_type {
            if let Some(v) = uri_value(o) {
                return v.starts_with(&wfns) || v == format!("{prov}Activity");
            }
        }
        false
    };
    let is_urn = |s: &str| s.starts_with("urn:sophia:wf");

    let stale: Vec<Triple> = cur
        .iter()
        .filter(|t| {
            !desired_canon.contains(&canon(t))
                && (is_managed_pred(&t.1, &t.2) || (is_urn(&t.0) && t.1.starts_with(&dcterms)))
        })
        .map(|t| (*t).clone())
        .collect();
    let missing: Vec<Triple> = desired
        .iter()
        .filter(|t| !cur_canon.contains(&canon(t)))
        .cloned()
        .collect();

    let rdf_delete = stale.len();
    let rdf_insert = missing.len();
    for u in render_updates("DELETE DATA", &stale, UPDATE_CHUNK) {
        steps.push(Step::SparqlUpdate { update: u });
    }
    for u in render_updates("INSERT DATA", &missing, UPDATE_CHUNK) {
        steps.push(Step::SparqlUpdate { update: u });
    }

    let summary = summarize(
        &steps,
        to_create.len(),
        to_delete.len(),
        rdf_delete,
        rdf_insert,
    );
    let mode = if runs_only {
        "runs-only"
    } else if existing.is_some() {
        "update"
    } else {
        "new"
    };
    Ok(Plan {
        graph: live.graph.clone(),
        workflow: name,
        vocab: contract.name.clone(),
        mode: mode.to_string(),
        short_id: short,
        workflow_doc_id: wf_doc_id,
        steps,
        summary,
        warnings,
        // The wf/campaign applier validates opt-in at reconcile, not here; the
        // memory-path SHACL gate is the only consumer of this field today.
        desired_inserts: Vec::new(),
        observer: String::new(),
        planned_at_ms: 0,
    })
}

// ---------------------------------------------------------------------------
// plan_campaign_compute — the optimization-campaign path.
// ---------------------------------------------------------------------------

/// Pure: `campaign + live snapshot → ordered op list` (same step grammar as
/// [`plan_compute`]). Idempotent by diffing. `campaign` is the typed record;
/// `campaign_json` is the same payload as loosely-typed JSON for the content
/// builders.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_campaign_compute(
    contract: &VocabularyContract,
    campaign: &CampaignRecord,
    campaign_json: &Json,
    live: &Live,
    current_triples: &[Triple],
    current_wires: &[CurrentWire],
    current_docs_md: &BTreeMap<String, String>,
    doc_provenance: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<Plan, PlanError> {
    let wfns = contract.primary_namespace().to_string();
    let mnemo = ns(contract, "mnemo").to_string();
    let prov = ns(contract, "prov").to_string();
    let rdf_type = format!("{}type", ns(contract, "rdf"));

    let cid = campaign.campaign_id.clone();
    let mut steps: Vec<Step> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let arch = live
        .archetypes
        .values()
        .find(|a| a.doc_id.as_deref() == Some(campaign.archetype_doc_id.as_str()))
        .cloned()
        .ok_or_else(|| {
            PlanError(format!(
                "archetype {} not found in graph (need its doc URI)",
                campaign.archetype_doc_id
            ))
        })?;
    let prefix = live.prefix.clone();
    if prefix.is_empty() {
        return Err(PlanError(
            "graph URI prefix unresolved — campaign filing requires a populated graph".to_string(),
        ));
    }
    let doc_uri = |d: &str| format!("{prefix}:doc:{d}");

    // folder: Agent Library/<name> · Population.
    let lib = live
        .folders
        .iter()
        .find(|(_, f)| f.label == "Agent Library" && f.parent_id.is_none())
        .map(|(fid, _)| fid.clone())
        .unwrap_or_else(|| "agent-library".to_string());
    let pop_folder = format!("{}-pop", campaign.archetype_doc_id);
    if !live.folders.contains_key(&pop_folder) {
        steps.push(Step::CreateFolder {
            folder_id: pop_folder.clone(),
            label: format!("{} · Population", campaign.archetype_name),
            parent_id: Some(lib),
        });
    }

    let slug_fn = |t: &str| slug(contract, t);

    // docs: variants + campaign record.
    let candidates = campaign_json
        .get("candidates")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let mut docs: Vec<(String, String)> = Vec::new();
    for c in &candidates {
        let idx = c.get("idx").and_then(Json::as_i64).unwrap_or(0);
        let text_len = c
            .get("text")
            .and_then(Json::as_str)
            .map(str::chars)
            .map(|c| c.count())
            .unwrap_or(0);
        if text_len > VARIANT_TEXT_CAP {
            warnings.push(format!(
                "cand#{idx} text {text_len}B exceeds cap {VARIANT_TEXT_CAP} \
                 — stored truncated in RDF, full in doc"
            ));
        }
        docs.push((
            variant_doc_id(&slug_fn, &cid, idx),
            variant_doc_content(campaign_json, c),
        ));
    }
    let record_doc = format!("{}-record", slug(contract, &cid));
    docs.push((record_doc.clone(), campaign_record_content(campaign_json)));

    for (did, content) in &docs {
        if !doc_converged(did, content, current_docs_md, doc_provenance, &mut warnings) {
            steps.push(Step::WriteDoc {
                doc_id: did.clone(),
                content: content.clone(),
                capture_script_block: None,
                capture_block_var: Some(format!("TB:{did}")),
            });
        }
        let cur_folder = live.docs.get(did).and_then(|d| d.folder_id.clone());
        if cur_folder.as_deref() != Some(pop_folder.as_str()) {
            steps.push(Step::Move {
                doc_id: did.clone(),
                folder_id: pop_folder.clone(),
            });
        }
    }

    // wires: derivedFrom lineage + exemplifies best -> archetype.
    let mut desired_wires: BTreeSet<(String, String, String)> = BTreeSet::new();
    for c in &candidates {
        let idx = c.get("idx").and_then(Json::as_i64).unwrap_or(0);
        if let Some(parent_idx) = c.get("parentIdx").and_then(Json::as_i64) {
            desired_wires.insert((
                format!("{wfns}derivedFrom"),
                variant_doc_id(&slug_fn, &cid, idx),
                variant_doc_id(&slug_fn, &cid, parent_idx),
            ));
        }
        if c.get("isBest").and_then(Json::as_bool).unwrap_or(false) {
            let arch_did = arch.doc_id.clone().unwrap_or_default();
            desired_wires.insert((
                format!("{mnemo}exemplifies"),
                variant_doc_id(&slug_fn, &cid, idx),
                arch_did,
            ));
        }
    }
    let mut have: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for w in current_wires {
        let key = (
            norm_pred(contract, &w.predicate),
            w.source_document_id.clone(),
            w.target_document_id.clone(),
        );
        have.entry(key).or_default().push(w.id.clone());
    }
    let to_create: Vec<(String, String, String)> = desired_wires
        .iter()
        .filter(|k| !have.contains_key(*k))
        .cloned()
        .collect();
    let mut to_delete: Vec<String> = Vec::new();
    for ids in have.values() {
        if ids.len() > 1 {
            to_delete.extend_from_slice(&ids[1..]);
        }
    }
    if !to_create.is_empty() {
        steps.push(Step::CreateWires {
            wires: to_create
                .iter()
                .map(|(p, s, t)| WireSpec {
                    predicate: p.clone(),
                    source_document_id: s.clone(),
                    target_document_id: t.clone(),
                })
                .collect(),
        });
    }
    if !to_delete.is_empty() {
        let sorted: BTreeSet<String> = to_delete.iter().cloned().collect();
        steps.push(Step::DeleteWires {
            wire_ids: sorted.into_iter().collect(),
        });
    }

    // RDF: campaign Run + Variants.
    let run_uri = format!("urn:sophia:wf-run:{cid}");
    let arch_uri = arch.uri.clone();
    let mut desired: Vec<Triple> = Vec::new();
    let started_ms = CAMPAIGN_EPOCH_MS;
    let wall_seconds = campaign_json
        .get("budget")
        .and_then(|b| b.get("wallSeconds"))
        .and_then(Json::as_i64)
        .unwrap_or(0);
    let task_calls = campaign_json
        .get("budget")
        .and_then(|b| b.get("taskCalls"))
        .and_then(Json::as_i64)
        .unwrap_or(0);
    let gate_passed = campaign_json
        .get("gate")
        .and_then(|g| g.get("passed"))
        .and_then(Json::as_bool)
        .unwrap_or(false);

    let mut rv: BTreeMap<String, FieldValue> = BTreeMap::new();
    put(&mut rv, "wf:runId", Value::Str(cid.clone()));
    put(
        &mut rv,
        "wf:workflowName",
        Value::Str(campaign.archetype_name.clone()),
    );
    put(&mut rv, "wf:status", Value::Str("completed".to_string()));
    put(&mut rv, "wf:totalTokens", Value::Int(0));
    put(
        &mut rv,
        "wf:agentCount",
        Value::Int(candidates.len() as i64),
    );
    put(&mut rv, "wf:durationMs", Value::Int(wall_seconds * 1000));
    put(&mut rv, "prov:used", Value::Uri(arch_uri.clone()));
    put(&mut rv, "prov:startedAtTime", Value::Int(started_ms));
    put(
        &mut rv,
        "prov:endedAtTime",
        Value::Int(started_ms + wall_seconds * 1000),
    );
    put(
        &mut rv,
        "wf:runRecordDocument",
        Value::Uri(doc_uri(&record_doc)),
    );
    put_opt(
        &mut rv,
        "wf:note",
        campaign.objective.clone().map(Value::Str),
    );
    put(
        &mut rv,
        "wf:campaignKind",
        Value::Str(campaign.kind.clone()),
    );
    put(&mut rv, "wf:targetArchetype", Value::Uri(arch_uri.clone()));
    put_opt(
        &mut rv,
        "wf:taskModel",
        campaign.task_model.clone().map(Value::Str),
    );
    put_opt(
        &mut rv,
        "wf:reflectionModel",
        campaign.reflection_model.clone().map(Value::Str),
    );
    put(
        &mut rv,
        "wf:seedScore",
        jdouble(campaign_json, &["seed", "train"]),
    );
    put(
        &mut rv,
        "wf:bestScore",
        jdouble(campaign_json, &["best", "train"]),
    );
    put(
        &mut rv,
        "wf:seedHoldout",
        jdouble(campaign_json, &["seed", "holdout"]),
    );
    put(
        &mut rv,
        "wf:bestHoldout",
        jdouble(campaign_json, &["best", "holdout"]),
    );
    put(&mut rv, "wf:budgetSpent", Value::Int(task_calls));
    put(
        &mut rv,
        "wf:gateVerdict",
        Value::Str(if gate_passed { "pass" } else { "fail" }.to_string()),
    );
    put(
        &mut rv,
        "wf:candidateCount",
        Value::Int(candidates.len() as i64),
    );
    desired.extend(render_class_triples(contract, &run_uri, "Run", &rv)?);

    for c in &candidates {
        let idx = c.get("idx").and_then(Json::as_i64).unwrap_or(0);
        let did = variant_doc_id(&slug_fn, &cid, idx);
        let text = c.get("text").and_then(Json::as_str).unwrap_or("");
        let capped: String = text.chars().take(VARIANT_TEXT_CAP).collect();
        let mut vv: BTreeMap<String, FieldValue> = BTreeMap::new();
        put(&mut vv, "wf:variantText", Value::Str(capped));
        put(&mut vv, "wf:textSha256", jstr_val(c, "sha256"));
        put(
            &mut vv,
            "wf:textBlock",
            Value::Placeholder(format!("TB:{did}")),
        );
        put(&mut vv, "wf:valScore", jval_double(c, "valScore"));
        put_opt(
            &mut vv,
            "wf:holdoutScore",
            c.get("holdoutScore")
                .and_then(Json::as_f64)
                .map(Value::Float),
        );
        put(&mut vv, "wf:generation", Value::Int(jint(c, "generation")));
        put(&mut vv, "wf:isSeed", Value::Bool(jbool(c, "isSeed")));
        put(
            &mut vv,
            "wf:isFrontier",
            Value::Bool(jbool(c, "isFrontier")),
        );
        put(&mut vv, "wf:isBest", Value::Bool(jbool(c, "isBest")));
        put(&mut vv, "wf:partOfCampaign", Value::Uri(run_uri.clone()));
        put(
            &mut vv,
            "wf:optimizesArchetype",
            Value::Uri(arch_uri.clone()),
        );
        put_opt(
            &mut vv,
            "prov:wasDerivedFrom",
            c.get("parentIdx")
                .and_then(Json::as_i64)
                .map(|pidx| Value::Uri(doc_uri(&variant_doc_id(&slug_fn, &cid, pidx)))),
        );
        desired.extend(render_class_triples(
            contract,
            &doc_uri(&did),
            "Variant",
            &vv,
        )?);
    }

    // ── diff RDF against live ──
    let managed: BTreeSet<&str> = desired.iter().map(|(s, _, _)| s.as_str()).collect();
    let cur: Vec<&Triple> = current_triples
        .iter()
        .filter(|(s, _, _)| managed.contains(s.as_str()))
        .collect();
    let desired_canon: BTreeSet<_> = desired.iter().map(canon).collect();
    let cur_canon: BTreeSet<_> = cur.iter().map(|t| canon(t)).collect();

    let bound_to_agent = format!("{wfns}boundToAgent");
    let is_managed = |p: &str, o: &Term| -> bool {
        if p == bound_to_agent {
            return false;
        }
        if p.starts_with(&wfns) || p.starts_with(&prov) {
            return true;
        }
        if p == rdf_type {
            if let Some(v) = uri_value(o) {
                return v.starts_with(&wfns) || v.contains("Activity");
            }
        }
        false
    };

    // textBlock handling: desired always carries a TB placeholder. A concrete
    // current value is correct UNLESS its doc is being rewritten (block ids change).
    let rewritten: BTreeSet<String> = steps
        .iter()
        .filter_map(|s| match s {
            Step::WriteDoc { doc_id, .. } => Some(doc_id.clone()),
            _ => None,
        })
        .collect();
    let subj_rewritten = |s: &str| -> bool {
        // re.search(r":doc:([^#]+)$", s)
        match s.rfind(":doc:") {
            Some(i) => {
                let tail = &s[i + ":doc:".len()..];
                if tail.is_empty() || tail.contains('#') {
                    false
                } else {
                    rewritten.contains(tail)
                }
            }
            None => false,
        }
    };
    let has_tb: BTreeSet<&str> = cur
        .iter()
        .filter(|(_, p, _)| p == &format!("{wfns}textBlock"))
        .map(|(s, _, _)| s.as_str())
        .collect();

    let textblock_pred = format!("{wfns}textBlock");
    let stale: Vec<Triple> = cur
        .iter()
        .filter(|t| {
            !desired_canon.contains(&canon(t))
                && is_managed(&t.1, &t.2)
                && (t.1 != textblock_pred || subj_rewritten(&t.0))
        })
        .map(|t| (*t).clone())
        .collect();
    let missing: Vec<Triple> = desired
        .iter()
        .filter(|t| {
            if cur_canon.contains(&canon(t)) {
                return false;
            }
            // suppress a TB placeholder insert when the graph already holds a
            // concrete textBlock for this subject and the doc is NOT rewritten.
            let is_tb_placeholder = placeholder_name(&t.2)
                .map(|n| n.starts_with("TB:"))
                .unwrap_or(false);
            !(is_tb_placeholder && has_tb.contains(t.0.as_str()) && !subj_rewritten(&t.0))
        })
        .cloned()
        .collect();

    let rdf_delete = stale.len();
    let rdf_insert = missing.len();
    for u in render_updates("DELETE DATA", &stale, UPDATE_CHUNK) {
        steps.push(Step::SparqlUpdate { update: u });
    }
    for u in render_updates("INSERT DATA", &missing, UPDATE_CHUNK) {
        steps.push(Step::SparqlUpdate { update: u });
    }

    let summary = summarize(
        &steps,
        to_create.len(),
        to_delete.len(),
        rdf_delete,
        rdf_insert,
    );
    Ok(Plan {
        graph: live.graph.clone(),
        workflow: campaign.archetype_name.clone(),
        vocab: contract.name.clone(),
        mode: "campaign".to_string(),
        short_id: slug(contract, &cid),
        workflow_doc_id: record_doc,
        steps,
        summary,
        warnings,
        desired_inserts: Vec::new(),
        observer: String::new(),
        planned_at_ms: 0,
    })
}

// ---------------------------------------------------------------------------
// plan_memory_compute — the sophia-memory-core path (vocab "memory").
// ---------------------------------------------------------------------------

/// The reserved memory projection root for minted subjects. For the shared
/// commons (empty observer) this is
/// `urn:mnemosyne:local:graph:{id}:projection:memory` — byte-identical to the
/// pre-per-agent behavior. For a per-OBSERVER witness it is the perspective graph
/// `…:projection:memory:agent:{observer}` (Variant B). This is BOTH the SUBJECT-IRI
/// root (records/sources/evidence live as subjects under it, so the survey's
/// `STRSTARTS` filter matches) AND the named GRAPH the apply fork wraps into — they
/// MUST agree, so both derive from the SAME observer.
fn memory_root(graph_id: &str, observer: &str) -> String {
    crate::rdf_authority::memory_projection_graph_iri_for(graph_id, observer)
}

/// `SourceReference` subject = `{root}:src:{sha256(sourceKind|sourceUri|externalId)}`
/// — dedupes shared sources across records. `sourceUri` is the best available URI
/// for the source (sourceBlock when a block, else externalUri), so two refs to the
/// same block collapse to one node. Scoped under the per-observer `memory_root`.
fn source_ref_id(graph_id: &str, observer: &str, sr: &SourceRefIn) -> String {
    let source_uri = source_block_uri(graph_id, sr)
        .or_else(|| sr.external_uri.clone())
        .unwrap_or_default();
    let external = sr.external_id.clone().unwrap_or_default();
    let h = sha256_text(&format!(
        "{}{HASH_SEP}{}{HASH_SEP}{}",
        sr.source_kind, source_uri, external
    ));
    format!("{}:src:{h}", memory_root(graph_id, observer))
}

/// The concrete CRDT block IRI for a DocumentBlock source ref, when both a
/// document and a block id are present: `{root-graph}:doc:{docId}#{blockId}`.
fn source_block_uri(graph_id: &str, sr: &SourceRefIn) -> Option<String> {
    match (&sr.document_id, &sr.block_id) {
        (Some(doc), Some(block)) if !doc.is_empty() && !block.is_empty() => {
            Some(format!("{}:doc:{doc}#{block}", graph_subject(graph_id)))
        }
        _ => None,
    }
}

/// `MemoryRecord` subject = `{root}:record:{memId}`. `memId` is the content hash
/// over the FROZEN input set (see [`MEMORY_PACK_VERSION`]):
/// `content | scope | contentOrientation | sorted(derivedFrom src IRIs) | observedAt
/// [| observer]`.
///
/// Variant A (observer-in-hash): the observer IRI is folded into the hash as an
/// ADDITIONAL segment so two witnesses' identical content mint DISTINCT subjects.
/// CONDITIONALITY (fix #2 — the "byte-for-byte" contract): the observer segment is
/// appended ONLY when the observer is non-empty. An empty observer reproduces the
/// pre-`@1.1.0` IRI byte-for-byte (the same 4 separators, no 5th), so every legacy
/// commons subject is preserved — pinned by the byte-identity regression test.
fn memory_record_id(
    graph_id: &str,
    observer: &str,
    r: &MemoryRecordIn,
    derived_from: &[String],
) -> String {
    let mut sources = derived_from.to_vec();
    sources.sort();
    let observed = r.observed_at.map(|v| v.to_string()).unwrap_or_default();
    let mut payload = format!(
        "{}{HASH_SEP}{}{HASH_SEP}{}{HASH_SEP}{}{HASH_SEP}{}",
        r.content,
        r.scope,
        r.content_orientation,
        sources.join(&HASH_SEP.to_string()),
        observed
    );
    // CONDITIONAL 5th segment: only when the observer is non-empty. Empty ⇒ the
    // payload is byte-identical to the pre-bump recipe (no trailing separator).
    if !observer.trim().is_empty() {
        payload.push(HASH_SEP);
        payload.push_str(observer.trim());
    }
    let h = sha256_text(&payload);
    format!("{}:record:{h}", memory_root(graph_id, observer))
}

/// `EvidenceLink` subject = `{root}:ev:{sha256(linkSubject|linkObject|linkType)}`.
/// Scoped under the per-observer `memory_root` (the link's subject IRI already
/// carries the observer via the record subject it names).
fn evidence_link_id(
    graph_id: &str,
    observer: &str,
    link_subject: &str,
    link_object: &str,
    link_type: &str,
) -> String {
    let h = sha256_text(&format!(
        "{link_subject}{HASH_SEP}{link_object}{HASH_SEP}{link_type}"
    ));
    format!("{}:ev:{h}", memory_root(graph_id, observer))
}

/// The OBSERVER (witness) a record is attributed to — the wire-supplied
/// `observer_agent_id`, trimmed. Empty/absent ⇒ the shared commons (`""`). This is
/// the single read-point for the observer key the whole memory path agrees on (the
/// subject root, the projection graph, the hash segment, and `mem:observedBy` all
/// derive from it).
pub(crate) fn record_observer(r: &MemoryRecordIn) -> &str {
    r.observer_agent_id.as_deref().map(str::trim).unwrap_or("")
}

/// The content-addressed `mem:MemoryRecord` subject a [`plan_memory_compute`]
/// run would mint for `r` — exposed so the `remember` front-end can echo the
/// minted subject in its result WITHOUT re-deriving the hash recipe. Single
/// source of truth: it rebuilds `derivedFrom` from the record's source refs via
/// [`source_ref_id`] (the same order/dedupe the planner uses) and hashes the
/// FROZEN field set behind [`MEMORY_PACK_VERSION`], keyed on the record's
/// [`record_observer`].
pub(crate) fn memory_record_subject(graph_id: &str, r: &MemoryRecordIn) -> String {
    let observer = record_observer(r);
    let derived_from: Vec<String> = r
        .source_refs
        .iter()
        .map(|sr| source_ref_id(graph_id, observer, sr))
        .collect();
    memory_record_id(graph_id, observer, r, &derived_from)
}

/// Pure: `memory records + live snapshot + current memory triples → ordered op
/// list`. Mirrors the diff→SparqlUpdate shape of [`plan_compute`] (the apply fork
/// graph-wraps the rendered updates into `:projection:memory` direct-on-store).
///
/// v1 invariants enforced here:
/// - content-addressed subjects (records / sources / evidence), frozen behind
///   [`MEMORY_PACK_VERSION`];
/// - the memory registry folder is self-created (gated on absence) so the FIRST
///   write on a clean graph never 400s (unlike the wf path's shortId prerequisite);
/// - conditional-required `confidence` (when an evidence relation is present) and
///   `validFrom` (when `isCurrent` is set) — planner-side `PlanError`;
/// - supersession edges when a same-content-orientation+scope head exists with a
///   different value (minimal v1 demote-old/new-head edges).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_memory_compute(
    contract: &VocabularyContract,
    graph_id: &str,
    records: &[MemoryRecordIn],
    live: &Live,
    current_memory_triples: &[Triple],
) -> Result<Plan, PlanError> {
    plan_memory_compute_at(
        contract,
        graph_id,
        records,
        live,
        current_memory_triples,
        chrono::Utc::now().timestamp_millis(),
    )
}

/// [`plan_memory_compute`] with an EXPLICIT clock. The plan records `now_ms` as
/// `planned_at_ms`; the memory event log captures it, and replaying the event
/// through this function with the same clock is deterministic — the disposability
/// oracle (projection(log) == store) rests on exactly this.
pub(crate) fn plan_memory_compute_at(
    contract: &VocabularyContract,
    graph_id: &str,
    records: &[MemoryRecordIn],
    live: &Live,
    current_memory_triples: &[Triple],
    now_ms: i64,
) -> Result<Plan, PlanError> {
    let now_iso = crate::emporium::terms::iso_from_ms(now_ms);
    let mut steps: Vec<Step> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // ── observer (Variant B) — the batch's witness ──
    // The whole plan writes into ONE per-observer projection graph (the apply fork
    // wraps every rendered update into it), so the batch MUST be observer-
    // homogeneous: a `remember` / `remember_batch` call is a single agent's act.
    // Derive the batch observer from the FIRST record and REJECT a mixed batch
    // loudly (rather than silently routing some records into the wrong graph).
    let observer = records
        .first()
        .map(record_observer)
        .unwrap_or("")
        .to_string();
    for r in records {
        if record_observer(r) != observer {
            return Err(PlanError(format!(
                "memory batch mixes observers ('{observer}' vs '{}') — one remember call \
                 must carry a single observer_agent_id (per-observer routing is graph-scoped)",
                record_observer(r)
            )));
        }
    }
    let observer = observer.as_str();

    // ── §8.2 lineage lookup: the stable chain id inherited through supersession ──
    // Read once from the live projection; used at mint time so a superseding
    // record adopts its target's lineage (or the target subject itself when the
    // target predates lineage minting), and a fresh assert roots a new lineage
    // at its own subject.
    let lineage_p = format!("{}lineage", contract.primary_namespace());
    let live_lineage: BTreeMap<&str, String> = current_memory_triples
        .iter()
        .filter(|(_, p, _)| p.as_str() == lineage_p)
        .filter_map(|(s, _, o)| {
            let nt = o.as_nt();
            nt.strip_prefix('<')
                .and_then(|x| x.strip_suffix('>'))
                .map(|u| (s.as_str(), u.to_string()))
        })
        .collect();

    // ── step 0: self-create the memory registry folder on a clean graph ──
    // Gated on absence so a converged re-file does not re-emit the folder op.
    const MEMORY_FOLDER_ID: &str = "memory";
    if !live.folders.contains_key(MEMORY_FOLDER_ID) {
        steps.push(Step::CreateFolder {
            folder_id: MEMORY_FOLDER_ID.to_string(),
            label: "Memory".to_string(),
            parent_id: None,
        });
    }

    // ── mint the desired triple set for every record + its provenance/evidence ──
    let mut desired: Vec<Triple> = Vec::new();
    // Dedupe source/evidence subjects across the batch (shared sources collapse).
    let mut seen_src: BTreeSet<String> = BTreeSet::new();
    let mut seen_ev: BTreeSet<String> = BTreeSet::new();
    // The set of record subjects this plan touches (the supersession demote pass
    // and the diff both scope to memory subjects under our root).
    let mut record_subjects: Vec<String> = Vec::new();

    for r in records {
        // 1) provenance — SourceReference nodes + the derivedFrom IRIs.
        let mut derived_from: Vec<String> = Vec::new();
        for sr in &r.source_refs {
            let src_subject = source_ref_id(graph_id, observer, sr);
            derived_from.push(src_subject.clone());
            if seen_src.insert(src_subject.clone()) {
                let mut sv: BTreeMap<String, FieldValue> = BTreeMap::new();
                put(
                    &mut sv,
                    "mem:sourceKind",
                    Value::Str(sr.source_kind.clone()),
                );
                put_opt(
                    &mut sv,
                    "mem:sourceLabel",
                    sr.source_label.clone().map(Value::Str),
                );
                put_opt(
                    &mut sv,
                    "mem:sourceBlock",
                    source_block_uri(graph_id, sr).map(Value::Uri),
                );
                put_opt(
                    &mut sv,
                    "mem:externalUri",
                    sr.external_uri.clone().map(Value::Uri),
                );
                put_opt(&mut sv, "mem:observedAt", sr.observed_at.map(Value::Int));
                put_opt(
                    &mut sv,
                    "mem:trustTier",
                    sr.trust_tier.clone().map(Value::Str),
                );
                desired.extend(render_class_triples(
                    contract,
                    &src_subject,
                    "SourceReference",
                    &sv,
                )?);
            }
        }
        if derived_from.is_empty() {
            // The I1 gate is also enforced at validate(); belt-and-suspenders so a
            // direct planner caller cannot mint a provenance-less record.
            return Err(PlanError(
                "memory record has no derivedFrom (I1 NO MEMORY WITHOUT PROVENANCE)".to_string(),
            ));
        }

        // 2) the record subject (content-addressed, observer-keyed).
        let subject = memory_record_id(graph_id, observer, r, &derived_from);
        record_subjects.push(subject.clone());

        // Conditional-required checks (planner-side, raising PlanError).
        if !r.evidence.is_empty() && r.confidence.is_none() {
            // An action/evidence relation is present → record-level confidence is
            // required (I4). (Evidence-level confidence is separately required on
            // the EvidenceLink class.)
            return Err(PlanError(format!(
                "memory record <{subject}> carries evidence but no record-level confidence (I4)"
            )));
        }
        if r.is_current.is_some() && r.valid_from.is_none() {
            return Err(PlanError(format!(
                "memory record <{subject}> sets isCurrent but no validFrom (I7)"
            )));
        }

        let mut mv: BTreeMap<String, FieldValue> = BTreeMap::new();
        put(&mut mv, "mem:content", Value::Str(r.content.clone()));
        put(&mut mv, "mem:scope", Value::Str(r.scope.clone()));
        put(&mut mv, "mem:kind", Value::Str(r.kind.clone()));
        put(
            &mut mv,
            "mem:contentOrientation",
            Value::Str(r.content_orientation.clone()),
        );
        put(&mut mv, "mem:visibility", Value::Str(r.visibility.clone()));
        put(&mut mv, "mem:status", Value::Str(r.status.clone()));
        put(&mut mv, "mem:graphId", Value::Str(graph_id.to_string()));
        // createdAt is set once at version mint. Use observedAt when present,
        // else now — and the re-file carry-forward below REPLACES this stamp
        // with the stored value for an already-live subject, so Case B
        // (content-hash re-file) converges to zero ops for observedAt-less
        // records too (the now() stamp only ever survives on the FIRST mint).
        let created_ms = r.observed_at.unwrap_or(now_ms);
        put(&mut mv, "mem:createdAt", Value::Int(created_ms));
        // mem:lineage — the stable chain id (§8.2 two-tier identity): the first
        // version's subject, inherited through supersession, so a fork becomes
        // the DETECTABLE state ">1 current head per lineage" (§8.1) rather than
        // an invisible pair of parallel heads. A supersession adopts its
        // target's lineage (falling back to the target subject when the target
        // predates lineage minting); a fresh assert roots the lineage at itself.
        let lineage = match r.supersedes_ref.as_deref().filter(|t| !t.is_empty()) {
            Some(target) => live_lineage
                .get(target)
                .cloned()
                .unwrap_or_else(|| target.to_string()),
            None => subject.clone(),
        };
        put(&mut mv, "mem:lineage", Value::Uri(lineage));
        put(
            &mut mv,
            "mem:createdByProcess",
            Value::Str("ingestion".to_string()),
        );
        // derivedFrom — the I1 provenance edges (multi, uri).
        mv.insert(
            "mem:derivedFrom".to_string(),
            FieldValue::Many(derived_from.iter().cloned().map(Value::Uri).collect()),
        );
        put_opt(&mut mv, "mem:observedAt", r.observed_at.map(Value::Int));
        put_opt(&mut mv, "mem:validFrom", r.valid_from.map(Value::Int));
        put_opt(&mut mv, "mem:isCurrent", r.is_current.map(Value::Bool));
        put_opt(&mut mv, "mem:confidence", r.confidence.map(Value::Float));
        put_opt(&mut mv, "mem:valence", r.valence.map(Value::Float));
        put_opt(&mut mv, "mem:agentId", r.agent_id.clone().map(Value::Str));
        // mem:observedBy (Variant B attribution) — the canonical OBSERVER predicate
        // (uri, OPTIONAL — NOT sh:minCount 1). The observer is the LEAF witness
        // IRI; emitted ONLY for a per-observer write (empty observer = the shared
        // commons keeps today's triple set byte-for-byte, no observedBy edge). The
        // wire id may be a bare `agent-<hex>` or a full IRI — serialized as a URI.
        if let Some(observer_iri) = crate::rdf_authority::observer_iri(observer) {
            put(&mut mv, "mem:observedBy", Value::Uri(observer_iri));
        }
        if !r.tags.is_empty() {
            mv.insert(
                "mem:tag".to_string(),
                FieldValue::Many(r.tags.iter().cloned().map(Value::Str).collect()),
            );
        }
        put_opt(
            &mut mv,
            "mem:supersedes",
            r.supersedes_ref.clone().map(Value::Uri),
        );
        if let Some(cref) = &r.contradicts_ref {
            mv.insert(
                "mem:contradicts".to_string(),
                FieldValue::Many(vec![Value::Uri(cref.clone())]),
            );
        }
        desired.extend(render_class_triples(
            contract,
            &subject,
            "MemoryRecord",
            &mv,
        )?);

        // 3) reified evidence — EvidenceLink nodes (attributes live ON the edge).
        for ev in &r.evidence {
            let Some(target) = ev.target_ref.clone().filter(|t| !t.is_empty()) else {
                warnings.push(format!(
                    "memory record <{subject}> evidence '{}' has no targetRef — skipped",
                    ev.relation
                ));
                continue;
            };
            let ev_subject = evidence_link_id(graph_id, observer, &subject, &target, &ev.relation);
            if !seen_ev.insert(ev_subject.clone()) {
                continue;
            }
            let confidence = ev.confidence.ok_or_else(|| {
                PlanError(format!(
                    "evidence link <{ev_subject}> missing required confidence (I2)"
                ))
            })?;
            let mut ev_values: BTreeMap<String, FieldValue> = BTreeMap::new();
            put(
                &mut ev_values,
                "mem:linkSubject",
                Value::Uri(subject.clone()),
            );
            put(&mut ev_values, "mem:linkObject", Value::Uri(target));
            put(
                &mut ev_values,
                "mem:linkType",
                Value::Str(ev.relation.clone()),
            );
            put(&mut ev_values, "mem:confidence", Value::Float(confidence));
            put_opt(
                &mut ev_values,
                "mem:evidenceStrength",
                ev.evidence_strength.map(Value::Float),
            );
            put_opt(
                &mut ev_values,
                "mem:trustTier",
                ev.trust_tier.clone().map(Value::Str),
            );
            desired.extend(render_class_triples(
                contract,
                &ev_subject,
                "EvidenceLink",
                &ev_values,
            )?);
        }
    }

    // ── PROV attribution of the perspective graph (Variant B) ──
    // For a per-observer write, attribute the perspective graph IRI itself to the
    // witness: `<perspectiveGraph> prov:wasAttributedTo <observer>`. The graph IRI
    // is the subject (the survey's no-trailing-colon prefix matches it, so this
    // CONVERGES — re-file emits 0 ops). `prov:generatedAtTime` is emitted ONLY when
    // the batch carries a deterministic observed time (the first record's
    // `observedAt`); a wall-clock now() would re-mint on every re-file, so it is
    // OMITTED otherwise (convergence over completeness). Commons (empty observer):
    // no attribution, today's triple set byte-for-byte.
    if let Some(observer_iri) = crate::rdf_authority::observer_iri(observer) {
        let perspective_graph = memory_root(graph_id, observer);
        let prov_attributed = contract.expand("prov:wasAttributedTo").map_err(PlanError)?;
        desired.push((
            perspective_graph.clone(),
            prov_attributed,
            crate::emporium::terms::term_for(
                &Value::Uri(observer_iri),
                crate::emporium::contract::Datatype::uri,
            ),
        ));
        if let Some(observed) = records.first().and_then(|r| r.observed_at) {
            let prov_generated = contract.expand("prov:generatedAtTime").map_err(PlanError)?;
            desired.push((
                perspective_graph,
                prov_generated,
                crate::emporium::terms::term_for(
                    &Value::Int(observed),
                    crate::emporium::contract::Datatype::dateTime,
                ),
            ));
        }
    }

    // ── supersession (v1, PRODUCER-DIRECTED): demote the head named by an
    // incoming record's `supersedesRef` ──
    // Supersession is a DELIBERATE lifecycle transition (§8.1 Case C), keyed on the
    // explicit `supersedesRef` the producer supplies — NOT a coarse
    // (scope, contentOrientation) auto-demote. The coarse key collided DISTINCT
    // memories that merely shared a scope+orientation (e.g. two different user
    // preferences), wrongly superseding one with the other. The new head's
    // `mem:supersedes <ref>` edge is minted above (from supersedesRef); here we flip
    // the referenced OLD head to superseded and write the `supersededBy`
    // back-pointer, carrying its content/provenance forward so the value-canonical
    // diff preserves it (append-only). The semantic decision of WHICH head a memory
    // supersedes is the producer's (the DeepSeek judgment pass) — the cell only
    // executes the transition it is told to.
    let incoming_subjects: BTreeSet<&str> = record_subjects.iter().map(String::as_str).collect();
    let mut demotions: Vec<Triple> = Vec::new();
    {
        let mem = contract.primary_namespace();
        let status_p = format!("{mem}status");
        let iscur_p = format!("{mem}isCurrent");
        let supby_p = format!("{mem}supersededBy");
        // Subjects that actually exist in the live projection — so we only demote a
        // real head and a dangling supersedesRef is a warning, not a phantom edit.
        let live_subjects: BTreeSet<&str> = current_memory_triples
            .iter()
            .map(|(s, _, _)| s.as_str())
            .collect();
        for r in records {
            let Some(target) = r.supersedes_ref.as_deref().filter(|t| !t.is_empty()) else {
                continue; // no supersession requested → mint as a new coexisting head.
            };
            let new_subject = memory_record_subject(graph_id, r);
            if target == new_subject.as_str() || incoming_subjects.contains(target) {
                continue; // can't supersede self or a sibling minted in this batch.
            }
            if !live_subjects.contains(target) {
                warnings.push(format!(
                    "memory record <{new_subject}> supersedesRef <{target}> is not a live record — supersedes edge kept, no demote"
                ));
                continue;
            }
            // Append-only demote: carry the OLD head's triples forward, overriding
            // ONLY status/isCurrent. `supersededBy` is deliberately CARRIED (not
            // excluded): a head superseded a second time keeps its existing
            // back-pointer(s) and gains the new one — successive supersessions
            // ACCUMULATE (the §8.1 fork state stays representable: H
            // supersededBy both H′ and H″, each holding a supersedes=H edge).
            // Excluding it here silently REPLACED the first writer's edge with
            // the second's, destroying the back-link while the first head still
            // claimed supersedes=H. Idempotent on re-file: every carried edge
            // (and the re-pushed one below) already matches the store, so the
            // value-canonical diff sees zero ops.
            for (s, p, o) in current_memory_triples.iter() {
                if s.as_str() == target && p.as_str() != status_p && p.as_str() != iscur_p {
                    demotions.push((s.clone(), p.clone(), o.clone()));
                }
            }
            demotions.push((
                target.to_string(),
                status_p.clone(),
                term_for_str("superseded"),
            ));
            demotions.push((
                target.to_string(),
                iscur_p.clone(),
                crate::emporium::terms::term_for(
                    &Value::Bool(false),
                    crate::emporium::contract::Datatype::boolean,
                ),
            ));
            demotions.push((
                target.to_string(),
                supby_p.clone(),
                crate::emporium::terms::term_for(
                    &Value::Uri(new_subject.clone()),
                    crate::emporium::contract::Datatype::uri,
                ),
            ));
            let _ = &now_iso; // validUntil stamping is a v2 refinement.
        }
    }
    desired.extend(demotions.iter().cloned());

    // Re-file convergence: the auto-`supersedes` edge (and any `supersededBy`) is
    // emitted by the demote pass, which SKIPS on a re-file (Case B). Without
    // re-deriving it, the value-canonical diff would treat an incoming head's
    // existing lifecycle edge as stale and DELETE it on every re-file. Carry
    // forward each incoming head's existing supersedes/supersededBy from the live
    // graph so re-filing a correction converges to zero ops.
    {
        let mem = contract.primary_namespace();
        let sup_p = format!("{mem}supersedes");
        let supby_p = format!("{mem}supersededBy");
        for (s, p, o) in current_memory_triples.iter() {
            if incoming_subjects.contains(s.as_str())
                && (p.as_str() == sup_p || p.as_str() == supby_p)
            {
                desired.push((s.clone(), p.clone(), o.clone()));
            }
        }
        // createdAt is SET ONCE at version mint: a re-file of a live subject
        // carries the STORED createdAt forward, REPLACING this plan's freshly
        // stamped value — so a content-hash re-file converges to zero ops even
        // for records without observedAt (whose first mint stamped now()).
        // Without this, every observedAt-less re-file churned one triple.
        let created_p = format!("{mem}createdAt");
        let live_created: Vec<Triple> = current_memory_triples
            .iter()
            .filter(|(s, p, _)| p.as_str() == created_p && incoming_subjects.contains(s.as_str()))
            .cloned()
            .collect();
        if !live_created.is_empty() {
            let carried: BTreeSet<&str> = live_created.iter().map(|(s, _, _)| s.as_str()).collect();
            desired.retain(|(s, p, _)| !(p.as_str() == created_p && carried.contains(s.as_str())));
            desired.extend(live_created);
        }
    }

    // ── diff against the live memory triples (scoped to our root subjects) ──
    let managed_subjects: BTreeSet<&str> = desired.iter().map(|(s, _, _)| s.as_str()).collect();
    let cur: Vec<&Triple> = current_memory_triples
        .iter()
        .filter(|(s, _, _)| managed_subjects.contains(s.as_str()))
        .collect();
    let desired_canon: BTreeSet<_> = desired.iter().map(canon).collect();
    let cur_canon: BTreeSet<_> = cur.iter().map(|t| canon(t)).collect();

    let stale: Vec<Triple> = cur
        .iter()
        .filter(|t| !desired_canon.contains(&canon(t)))
        .map(|t| (*t).clone())
        .collect();
    let missing: Vec<Triple> = desired
        .iter()
        .filter(|t| !cur_canon.contains(&canon(t)))
        .cloned()
        .collect();

    let rdf_delete = stale.len();
    let rdf_insert = missing.len();
    for u in render_updates("DELETE DATA", &stale, UPDATE_CHUNK) {
        steps.push(Step::SparqlUpdate { update: u });
    }
    for u in render_updates("INSERT DATA", &missing, UPDATE_CHUNK) {
        steps.push(Step::SparqlUpdate { update: u });
    }

    let summary = summarize(&steps, 0, 0, rdf_delete, rdf_insert);
    // T5.22: the mode this plan routes on is DERIVED from the contract's own
    // declared signature (via the canonical `MemoryRecord` class), not a bare
    // "memory" literal — `Plan::routes_to_memory_sink` reads this same
    // `DispatchRoute` label back. `contract` is always `memory_core_vocabulary()`
    // in production (the ONE caller, `spine::gather_and_plan`'s memory branch),
    // so this is a no-op today; a synthetic contract in a test can flip
    // `MemoryRecord`'s declared `store_target` and observe the mode move.
    let mode = class_dispatch::resolve(contract, "MemoryRecord")
        .map_err(PlanError)?
        .route
        .mode_label()
        .to_string();
    Ok(Plan {
        graph: live.graph.clone(),
        workflow: records
            .first()
            .and_then(|r| r.client_ref.clone())
            .unwrap_or_else(|| "memory-batch".to_string()),
        vocab: contract.name.clone(),
        mode,
        short_id: MEMORY_PACK_VERSION.to_string(),
        workflow_doc_id: String::new(),
        steps,
        summary,
        warnings,
        // The DESIRED INSERT set (EA-6): the exact NEW typed triples this plan will
        // materialize, retained for the memory-path SHACL gate in `apply_memory_plan`.
        // These are the same `Triple`s rendered into the `INSERT DATA` chunks above
        // (`missing`), so the validator sees byte-faithful terms without re-parsing
        // N-Triples. DELETE/demote triples reference already-valid live state and are
        // not validated. On a converged re-file `missing` is empty → the gate's
        // `validate_desired_structured([])` conforms trivially.
        desired_inserts: missing,
        observer: observer.to_string(),
        planned_at_ms: now_ms,
    })
}

/// A simple xsd:string term (for status/etc. literal edges minted outside the
/// `render_class_triples` path, e.g. the supersession demote-old-head triples).
fn term_for_str(s: &str) -> Term {
    crate::emporium::terms::term_for(
        &Value::Str(s.to_string()),
        crate::emporium::contract::Datatype::string,
    )
}

// ---------------------------------------------------------------------------
// The GENERIC simple-projection planner (EA-3 / B5 + B4 plan half).
//
// `plan_generic_compute` is the vocab-AGNOSTIC mint: for ANY registered vocab
// whose contract declares a `projection:*` write_target, it parses each class's
// `subject_rule` (B5 grammar), mints a subject per record, and renders the
// class triples through the SAME frozen-vocab-guarded `render_class_triples`
// primitive the wf/memory planners use. The result is a `Plan` with
// `mode = "simple-projection"` and the full desired triple set in
// `desired_inserts` — the apply fork (spine) routes that mode to
// `reconcile_class_validated`, which surveys the projection sink, diffs (zero
// ops on a converged re-ingest), runs the SHACL gate, and applies. The planner
// emits NO `SparqlUpdate` steps: the reconcile primitive owns the diff/apply, so
// idempotency + the convergence oracle come straight from EA-1's proven path.
// ---------------------------------------------------------------------------

/// Coerce a raw ingest JSON value into the typed [`Value`] for a declared
/// [`Datatype`]. Mirrors the duck-typing `term_for` expects: object datatypes
/// (`uri`) carry a string IRI; numerics accept JSON numbers or numeric strings;
/// `dateTime` accepts epoch-ms (number) or an ISO string; `boolean` accepts a JSON
/// bool. Returns `None` for JSON `null` (treated as "not provided").
fn generic_value(json: &Json, datatype: crate::emporium::contract::Datatype) -> Option<Value> {
    use crate::emporium::contract::Datatype;
    if json.is_null() {
        return None;
    }
    Some(match datatype {
        Datatype::uri => Value::Uri(
            json.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| json.to_string()),
        ),
        Datatype::string => Value::Str(
            json.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| json.to_string()),
        ),
        Datatype::integer | Datatype::long => {
            if let Some(i) = json.as_i64() {
                Value::Int(i)
            } else if let Some(s) = json.as_str() {
                Value::Int(s.parse().unwrap_or(0))
            } else {
                Value::Int(json.as_f64().map(|f| f as i64).unwrap_or(0))
            }
        }
        Datatype::double | Datatype::float => {
            if let Some(f) = json.as_f64() {
                Value::Float(f)
            } else if let Some(s) = json.as_str() {
                Value::Float(s.parse().unwrap_or(0.0))
            } else {
                Value::Float(0.0)
            }
        }
        Datatype::boolean => Value::Bool(json.as_bool().unwrap_or(false)),
        Datatype::dateTime => {
            if let Some(ms) = json.as_i64() {
                Value::Int(ms) // term_for(dateTime, Int) → ISO from epoch-ms.
            } else {
                Value::Str(json.as_str().map(str::to_string).unwrap_or_default())
            }
        }
    })
}

/// Resolve a record field key against a class predicate CURIE: a field matches if
/// it equals the full CURIE (`bm:url`) OR the predicate's local name (`url`). The
/// local-name form is the ergonomic ingest shape; the CURIE form is unambiguous.
fn field_for_predicate<'a>(
    fields: &'a serde_json::Map<String, Json>,
    curie: &str,
) -> Option<&'a Json> {
    if let Some(v) = fields.get(curie) {
        return Some(v);
    }
    let local = curie.split_once(':').map(|(_, l)| l).unwrap_or(curie);
    fields.get(local)
}

/// Pure: `generic records + the registered contract + graph_id → a
/// simple-projection Plan`. The mint is the B5/B4 plan half:
///   1. parse the class `subject_rule` (Template/Descriptive); Descriptive is
///      rejected (the generic path has no per-class code-mint default);
///   2. mint the subject by interpolating `{graph_subject}` + `{localId}` (+ any
///      record-supplied token) into the Template pattern;
///   3. map the record's flat fields onto the class predicate CURIEs (CURIE or
///      local-name keys), coerce each per its declared datatype;
///   4. `render_class_triples` (frozen-vocab guard: a missing required predicate
///      or a rogue key is a `PlanError`).
/// The full desired set goes into `desired_inserts`; the apply fork reconciles it.
pub(crate) fn plan_generic_compute(
    contract: &VocabularyContract,
    graph_id: &str,
    records: &[crate::emporium::schemas::GenericRecordIn],
) -> Result<Plan, PlanError> {
    use crate::emporium::subject_rule::{
        mint_subject_from_rule, parse_subject_rule, TOKEN_GRAPH_SUBJECT,
    };

    let graph_subject = graph_subject(graph_id);
    let mut desired: Vec<Triple> = Vec::new();
    // De-dupe identical subjects across the batch (a re-listed localId collapses to
    // one subject — last write wins on its predicate values within render order).
    let mut seen_subjects: BTreeSet<String> = BTreeSet::new();

    for (idx, record) in records.iter().enumerate() {
        if contract.name == "garden-file-views" {
            let value = serde_json::to_value(record)
                .map_err(|error| PlanError(format!("folder view record: {error}")))?;
            crate::emporium::folder_views::validate_folder_view_record(&value, graph_id)
                .map_err(PlanError)?;
        }
        let class_name = &record.kind;
        let spec = contract.classes.get(class_name).ok_or_else(|| {
            PlanError(format!(
                "generic record [{idx}]: unknown class {class_name}"
            ))
        })?;
        // T5.22: re-routed through class_dispatch — the SAME (contract, class)
        // -> route resolution `materialized_class_partitions` uses, so the
        // generic planner's virtual rejection and the simple-projection
        // applier's materialize/virtual partition read one declaration, not two
        // independent `store_mode` comparisons.
        let dispatch = class_dispatch::resolve(contract, class_name).map_err(PlanError)?;
        if dispatch.route == class_dispatch::DispatchRoute::VirtualSkip {
            return Err(PlanError(format!(
                "generic record [{idx}]: class {class_name} is store_mode=virtual; \
                 it is resolved on read via derived_from_query and cannot be materialized"
            )));
        }

        // ── B5: parse + mint the subject from the class subject_rule ──
        let rule_str = spec.subject_rule.as_deref().ok_or_else(|| {
            PlanError(format!(
                "generic record [{idx}]: class {class_name} has no subject_rule (cannot mint a subject)"
            ))
        })?;
        let rule = parse_subject_rule(rule_str).map_err(PlanError)?;

        let local_id = record
            .fields
            .get("localId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                PlanError(format!(
                    "generic record [{idx}]: missing 'localId' for {class_name}"
                ))
            })?;
        let mut tokens: BTreeMap<String, String> = BTreeMap::new();
        tokens.insert(TOKEN_GRAPH_SUBJECT.to_string(), graph_subject.clone());
        // `graphId` is cell authority, not caller-authored record content.  A
        // vocabulary may nevertheless use it as part of a deterministic
        // subject template (workflow CompositionEvent does).  Bind it from the
        // graph seam just as we bind `{graph_subject}`; requiring clients to
        // repeat it in each record would permit a record to mint into the wrong
        // graph namespace.
        tokens.insert("graphId".to_string(), graph_id.to_string());
        tokens.insert("localId".to_string(), local_id.to_string());
        // Allow any other record field to bind a same-named token (e.g. a custom
        // `{slug}` in a product's subject_rule). String/number scalars only.
        for (key, value) in &record.fields {
            if key == "localId" {
                continue;
            }
            if let Some(s) = value.as_str() {
                tokens.entry(key.clone()).or_insert_with(|| s.to_string());
            } else if value.is_number() || value.is_boolean() {
                tokens
                    .entry(key.clone())
                    .or_insert_with(|| value.to_string());
            }
        }
        let subject = mint_subject_from_rule(&rule, &tokens).map_err(PlanError)?;

        // ── no-silent-drop guard: every record field must be KNOWN ──
        // A field is legal iff it is `localId`, a declared predicate (matched by
        // CURIE or local name), or a token referenced by the class subject_rule.
        // A field outside this set is a LOUD `PlanError` (a `url` typo, a rogue
        // `bm:bogus`, …) — never silently ignored. This mirrors the memory path's
        // `scan_unknown_memory_keys`: a misspelled key cannot quietly strip data.
        let rule_tokens: BTreeSet<&str> = match &rule {
            crate::emporium::subject_rule::SubjectRule::Template {
                required_tokens, ..
            } => required_tokens.iter().map(String::as_str).collect(),
            crate::emporium::subject_rule::SubjectRule::Descriptive { .. } => BTreeSet::new(),
        };
        let predicate_keys: BTreeSet<String> = spec
            .predicates
            .keys()
            .flat_map(|curie| {
                let local = curie.split_once(':').map(|(_, l)| l).unwrap_or(curie);
                [curie.clone(), local.to_string()]
            })
            .collect();
        let unknown: Vec<&str> = record
            .fields
            .keys()
            .map(String::as_str)
            .filter(|k| {
                *k != "localId" && !predicate_keys.contains(*k) && !rule_tokens.contains(*k)
            })
            .collect();
        if !unknown.is_empty() {
            return Err(PlanError(format!(
                "generic record [{idx}] ({class_name}): unknown field(s) {unknown:?} — not a \
                 declared predicate of vocab '{}' (refusing to silently drop; check for a typo)",
                contract.name
            )));
        }

        // ── map the flat record fields onto the class predicate CURIEs ──
        let mut values: BTreeMap<String, FieldValue> = BTreeMap::new();
        for (curie, pspec) in &spec.predicates {
            let Some(raw) = field_for_predicate(&record.fields, curie) else {
                continue; // absent — render_class_triples enforces required-ness.
            };
            if pspec.multi {
                // A multi predicate accepts a JSON array (one value per element) or
                // a lone scalar (a single-element list).
                let items: Vec<Value> = match raw {
                    Json::Array(arr) => arr
                        .iter()
                        .filter_map(|v| generic_value(v, pspec.datatype))
                        .collect(),
                    other => generic_value(other, pspec.datatype).into_iter().collect(),
                };
                if !items.is_empty() {
                    values.insert(curie.clone(), FieldValue::Many(items));
                }
            } else if let Some(v) = generic_value(raw, pspec.datatype) {
                values.insert(curie.clone(), FieldValue::One(v));
            }
        }

        let triples = render_class_triples(contract, &subject, class_name, &values)?;
        // Collapse duplicate subjects: skip a subject already rendered this batch.
        if seen_subjects.insert(subject.clone()) {
            desired.extend(triples);
        }
    }

    let summary = summarize(&[], 0, 0, 0, desired.len());
    let subject_name = records
        .first()
        .and_then(|r| r.fields.get("localId"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| "generic-batch".to_string());

    Ok(Plan {
        graph: graph_id.to_string(),
        workflow: subject_name,
        vocab: contract.name.clone(),
        // The apply-dispatch signal: the spine routes this to the generic
        // simple-projection applier (reconcile into write_target). MUST agree with
        // the contract's write_target (the spine loud-halts on a mismatch).
        mode: "simple-projection".to_string(),
        short_id: format!("{}@{}", contract.name, contract.version),
        workflow_doc_id: String::new(),
        steps: Vec::new(),
        summary,
        warnings: Vec::new(),
        // The full desired projection state. The reconcile applier diffs this
        // against the live sink (zero ops on a converged re-ingest), SHACL-validates
        // it, and applies only the delta.
        desired_inserts: desired,
        // Generic simple-projection is not a per-observer memory plan — shared.
        observer: String::new(),
        planned_at_ms: 0,
    })
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn summarize(
    steps: &[Step],
    wires_create: usize,
    wires_delete: usize,
    rdf_delete: usize,
    rdf_insert: usize,
) -> PlanSummary {
    PlanSummary {
        folders: steps
            .iter()
            .filter(|s| s.op_name() == "create_folder")
            .count(),
        doc_writes: steps.iter().filter(|s| s.op_name() == "write_doc").count(),
        moves: steps.iter().filter(|s| s.op_name() == "move").count(),
        wires_create,
        wires_delete,
        rdf_delete,
        rdf_insert,
    }
}

/// First 12 chars of a sha (mirrors Python `sha[:12]`). Char-safe (shas are hex).
fn short_sha(s: &str) -> String {
    s.chars().take(12).collect()
}

/// The object value of a current triple, as a `Value` suitable for re-minting
/// (mirrors reading `o.value` off a pyoxigraph term). URIs/placeholders keep
/// their URI shape; literals become strings.
fn term_object_value(o: &Term) -> Option<Value> {
    match o {
        Term::Uri(n) => Some(Value::Uri(n.as_str().to_string())),
        Term::Placeholder(name) => Some(Value::Placeholder(name.clone())),
        Term::Lit(l) => Some(Value::Str(l.value().to_string())),
    }
}

/// The URI string of a term if it is a URI (else None). Used for rdf:type object
/// checks in the managed-predicate filter.
fn uri_value(o: &Term) -> Option<String> {
    match o {
        Term::Uri(n) => Some(n.as_str().to_string()),
        _ => None,
    }
}

/// The placeholder name of a term if it is a placeholder (mirrors
/// `terms.placeholder_name`).
fn placeholder_name(o: &Term) -> Option<String> {
    match o {
        Term::Placeholder(name) => Some(name.clone()),
        _ => None,
    }
}

fn jint(v: &Json, key: &str) -> i64 {
    v.get(key).and_then(Json::as_i64).unwrap_or(0)
}

fn jbool(v: &Json, key: &str) -> bool {
    v.get(key).and_then(Json::as_bool).unwrap_or(false)
}

fn jstr_val(v: &Json, key: &str) -> Value {
    Value::Str(v.get(key).and_then(Json::as_str).unwrap_or("").to_string())
}

fn jval_double(v: &Json, key: &str) -> Value {
    Value::Float(v.get(key).and_then(Json::as_f64).unwrap_or(0.0))
}

fn jdouble(v: &Json, path: &[&str]) -> Value {
    let mut cur = v;
    for k in path {
        match cur.get(k) {
            Some(next) => cur = next,
            None => return Value::Float(0.0),
        }
    }
    Value::Float(cur.as_f64().unwrap_or(0.0))
}

#[cfg(test)]
mod tests;
