//! Contract assertion suite — executable verification, generated from the vocab.
//!
//! Port of `emporium_engine/ingest/asserts.py::assert_workflow`. PURE: it takes a
//! snapshot of graph state (typed triples, wires, doc markdown) and returns a
//! pass/fail [`AssertReport`]. The I/O wrapper that gathers the snapshot lives in
//! the spine (`apply_and_assert`, the workflow kind); the tests feed synthetic
//! snapshots directly.
//!
//! Checks (the required-predicate sweep is GENERATED from the contract's
//! `required` flags — the SAME flags [`mint::render_class_triples`] enforces, so
//! the suite follows the vocab; change the vocab and the suite follows):
//!   1. the workflow exists and its stored script sha round-trips from the
//!      ```` ```javascript ```` doc fence (tolerating the newline candidates);
//!   2. required predicates per class instance, generated from the contract;
//!   3. AgentRun → node links target this workflow's nodes;
//!   4. duplicate wires (same predicate/source/target);
//!   5. legacy-namespace drift (`http://sophia.dev/workflow#`);
//!   6. rogue-predicate scan: primary-namespace predicates outside the contract
//!      (known class predicates ∪ wire predicate_uris);
//!   7. (non-fatal) managed-doc drift: live content vs recorded
//!      `emp:renderSha256` — a hand-edit is legitimate, recoverable behaviour the
//!      next ingest reports and overwrites, so it lands in `warnings`, NEVER
//!      `failures`.
//!
//! Spec critique addition (vs the platform source): the required-predicate sweep
//! also discovers `Contract` and `Archetype` instances, so those classes get
//! provenance-time integrity (not just plan-time minting). A `Contract`/
//! `Archetype` subject is one carrying that class's `rdf:type`.

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;

use crate::emporium::contract::VocabularyContract;
use crate::emporium::terms::{canonical_md, sha256_text, Term, Triple};

/// Namespaces this vocabulary once lived in; any surviving triple is drift.
const LEGACY_NAMESPACES: &[&str] = &["http://sophia.dev/workflow#"];

/// The fenced `javascript` code block whose body re-hashes to `wf:scriptSha256`.
/// Mirrors the Python `_FENCE = r"(?m)^```javascript\n([\s\S]*?)\n^```\s*$"`.
fn fence_re() -> Regex {
    Regex::new(r"(?m)^```javascript\n([\s\S]*?)\n^```[ \t]*$").expect("fence regex is valid")
}

/// The pass/fail report. Mirrors the Python `{"pass", "failures", "warnings",
/// "stats"}` dict; `pass` is JSON-serialized as `pass` (a reserved Rust word, so
/// the field is `passed` with a serde rename).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AssertReport {
    #[serde(rename = "pass")]
    pub(crate) passed: bool,
    pub(crate) failures: Vec<String>,
    pub(crate) warnings: Vec<String>,
    pub(crate) stats: AssertStats,
}

/// The `stats` block (mirrors the Python dict: counts + the stored sha). All
/// fields skip-serialize when absent so an early-return report (no wf:Workflow)
/// emits `"stats": {}` like the source.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct AssertStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) nodes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) runs: Option<usize>,
    #[serde(rename = "agentRuns", skip_serializing_if = "Option::is_none")]
    pub(crate) agent_runs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) phases: Option<usize>,
    #[serde(rename = "wireCombos", skip_serializing_if = "Option::is_none")]
    pub(crate) wire_combos: Option<usize>,
}

/// A wire as the survey hands it to the suite (`{id, predicate,
/// sourceDocumentId, targetDocumentId}`). The duplicate-wire combo key is
/// `(predicate, sourceDocumentId, targetDocumentId)`. Mirrors the Python wire
/// dicts; any field may be absent (`None`) to match `w.get(...)`.
#[derive(Debug, Clone, Default)]
pub(crate) struct AssertWire {
    pub(crate) id: String,
    pub(crate) predicate: Option<String>,
    pub(crate) source_document_id: Option<String>,
    pub(crate) target_document_id: Option<String>,
}

/// The object value of a term, mirroring pyoxigraph's `term.value`: a NamedNode's
/// value is its URI; a Literal's is its lexical form; a placeholder is its
/// reserved URN (so `==` against a concrete URI never spuriously matches).
fn term_value(o: &Term) -> String {
    match o {
        Term::Uri(n) => n.as_str().to_string(),
        Term::Lit(l) => l.value().to_string(),
        Term::Placeholder(name) => {
            format!("{}{}", crate::emporium::terms::PLACEHOLDER_NS, name)
        }
    }
}

/// First 12 chars of a sha (mirrors Python `sha[:12]`). Hex shas are ASCII, so
/// this is char-safe.
fn short12(s: &str) -> String {
    s.chars().take(12).collect()
}

/// Assert a workflow's anatomy against the contract. PURE — see the module docs.
///
/// `triples` is the workflow-managed triple snapshot (the survey's
/// `current_wf_triples` shape); `wires` the workspace wires; `workflow_doc_md`
/// the workflow doc's canonical markdown (for the sha round-trip); `docs_md` /
/// `doc_provenance` the optional managed-doc drift inputs (`docId -> markdown`
/// and `docId -> {renderSha256, …}`).
#[allow(dead_code)]
pub(crate) fn assert_workflow(
    contract: &VocabularyContract,
    name: &str,
    triples: &[Triple],
    wires: &[AssertWire],
    workflow_doc_md: Option<&str>,
    docs_md: Option<&BTreeMap<String, String>>,
    doc_provenance: Option<&BTreeMap<String, BTreeMap<String, String>>>,
) -> AssertReport {
    let wfns = contract.primary_namespace().to_string();
    let prov = contract.namespaces.get("prov").cloned().unwrap_or_default();
    let rdf_type = format!(
        "{}type",
        contract.namespaces.get("rdf").cloned().unwrap_or_default()
    );

    let mut failures: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut stats = AssertStats::default();

    // Index triples by subject → predicate → [objects], mirroring `by_subj`.
    // BTreeMap keeps a deterministic subject order (the Python dict preserves
    // insertion order; for the failure messages we want a stable order anyway).
    let mut by_subj: BTreeMap<&str, BTreeMap<&str, Vec<&Term>>> = BTreeMap::new();
    for (s, p, o) in triples {
        by_subj
            .entry(s.as_str())
            .or_default()
            .entry(p.as_str())
            .or_default()
            .push(o);
    }

    // Locate the wf:Workflow URI by name + rdf:type. `next(...)` semantics: the
    // first matching subject (deterministic over the sorted subject set).
    let name_pred = format!("{wfns}name");
    let workflow_type = format!("{wfns}Workflow");
    let wf_uri: Option<String> = by_subj.iter().find_map(|(s, ps)| {
        let named = ps
            .get(name_pred.as_str())
            .map(|os| os.iter().any(|o| term_value(o) == name))
            .unwrap_or(false);
        let typed = ps
            .get(rdf_type.as_str())
            .map(|os| os.iter().any(|o| term_value(o) == workflow_type))
            .unwrap_or(false);
        if named && typed {
            Some((*s).to_string())
        } else {
            None
        }
    });
    let Some(wf_uri) = wf_uri else {
        return AssertReport {
            passed: false,
            failures: vec![format!("no wf:Workflow named '{name}'")],
            warnings: Vec::new(),
            stats: AssertStats::default(),
        };
    };

    // Stored script sha.
    let sha: Option<String> = by_subj
        .get(wf_uri.as_str())
        .and_then(|ps| ps.get(format!("{wfns}scriptSha256").as_str()))
        .and_then(|os| os.first())
        .map(|o| term_value(o));
    stats.sha = sha.clone();

    // 1. sha round-trip from the doc fence.
    match workflow_doc_md {
        None => failures.push("workflow doc markdown unavailable for sha round-trip".to_string()),
        Some(md) => match fence_re().captures(md) {
            None => failures.push("workflow doc: no ```javascript fence found".to_string()),
            Some(caps) => match &sha {
                None => failures.push("wf:scriptSha256 missing".to_string()),
                Some(sha) => {
                    let body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let no_trailing = body.trim_end_matches('\n');
                    let cands: BTreeSet<String> = [
                        body.to_string(),
                        format!("{body}\n"),
                        no_trailing.to_string(),
                        format!("{no_trailing}\n"),
                    ]
                    .into_iter()
                    .collect();
                    let recovered: BTreeSet<String> =
                        cands.iter().map(|c| sha256_text(c)).collect();
                    if !recovered.contains(sha) {
                        failures.push(format!(
                            "sha round-trip MISMATCH: stored {}, recovered {}",
                            short12(sha),
                            short12(&sha256_text(body))
                        ));
                    }
                }
            },
        },
    }

    // Subject sets per class (mirrors the comprehensions over `by_subj`).
    let part_of_workflow = format!("{wfns}partOfWorkflow");
    let prov_used = format!("{prov}used");
    let part_of_run = format!("{wfns}partOfRun");
    let phase_pred = format!("{wfns}phase");
    let node_pred = format!("{wfns}node");

    let nodes: Vec<String> = by_subj
        .iter()
        .filter(|(_, ps)| {
            ps.get(part_of_workflow.as_str())
                .map(|os| os.iter().any(|o| term_value(o) == wf_uri))
                .unwrap_or(false)
        })
        .map(|(s, _)| (*s).to_string())
        .collect();
    let runs: Vec<String> = by_subj
        .iter()
        .filter(|(_, ps)| {
            ps.get(prov_used.as_str())
                .map(|os| os.iter().any(|o| term_value(o) == wf_uri))
                .unwrap_or(false)
        })
        .map(|(s, _)| (*s).to_string())
        .collect();
    let run_set: BTreeSet<String> = runs.iter().cloned().collect();
    let agent_runs: Vec<String> = by_subj
        .iter()
        .filter(|(_, ps)| {
            ps.get(part_of_run.as_str())
                .map(|os| os.iter().any(|o| run_set.contains(&term_value(o))))
                .unwrap_or(false)
        })
        .map(|(s, _)| (*s).to_string())
        .collect();
    let phases: Vec<String> = by_subj
        .get(wf_uri.as_str())
        .and_then(|ps| ps.get(phase_pred.as_str()))
        .map(|os| os.iter().map(|o| term_value(o)).collect())
        .unwrap_or_default();

    stats.nodes = Some(nodes.len());
    stats.runs = Some(runs.len());
    stats.agent_runs = Some(agent_runs.len());
    stats.phases = Some(phases.len());

    // 2. required predicates per class instance (generated from the contract).
    // `check` sweeps a subject for every predicate the class marks `required`.
    let check = |cls: &str, subj: &str, failures: &mut Vec<String>| {
        let Some(spec) = contract.classes.get(cls) else {
            return;
        };
        let ps = by_subj.get(subj);
        for (curie, pspec) in &spec.predicates {
            if !pspec.required {
                continue;
            }
            let Ok(uri) = contract.expand(curie) else {
                continue;
            };
            let present = ps.map(|m| m.contains_key(uri.as_str())).unwrap_or(false);
            if !present {
                failures.push(format!("{cls} <{subj}> missing required {curie}"));
            }
        }
    };

    check("Workflow", &wf_uri, &mut failures);
    for s in &phases {
        check("Phase", s, &mut failures);
    }
    for s in &nodes {
        check("AgentNode", s, &mut failures);
    }
    for s in &runs {
        check("Run", s, &mut failures);
    }
    let node_set: BTreeSet<&str> = nodes.iter().map(String::as_str).collect();
    for s in &agent_runs {
        check("AgentRun", s, &mut failures);
        // AgentRun wf:node → must be an AgentNode of THIS workflow.
        if let Some(node_t) = by_subj
            .get(s.as_str())
            .and_then(|ps| ps.get(node_pred.as_str()))
            .and_then(|os| os.first())
        {
            let target = term_value(node_t);
            if !node_set.contains(target.as_str()) {
                failures.push(format!(
                    "AgentRun <{s}> wf:node -> not an AgentNode of this workflow: {target}"
                ));
            }
        }
    }

    // Spec critique: provenance-time integrity for the new classes. Discover
    // Contract / Archetype instances by their rdf:type and sweep their required
    // predicates too (plan-time minting already enforces them; this re-checks at
    // assert time, the same way the core classes are re-checked).
    for cls in ["Contract", "Archetype"] {
        let Some(type_uri) = contract
            .classes
            .get(cls)
            .and_then(|spec| spec.rdf_types.first())
            .and_then(|t| contract.expand(t).ok())
        else {
            continue;
        };
        let subjects: Vec<String> = by_subj
            .iter()
            .filter(|(_, ps)| {
                ps.get(rdf_type.as_str())
                    .map(|os| os.iter().any(|o| term_value(o) == type_uri))
                    .unwrap_or(false)
            })
            .map(|(s, _)| (*s).to_string())
            .collect();
        for s in &subjects {
            check(cls, s, &mut failures);
        }
    }

    // 3. duplicate wires (same predicate/source/target combo with >1 wire id).
    let mut combos: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for w in wires {
        let key = (
            w.predicate.clone().unwrap_or_default(),
            w.source_document_id.clone().unwrap_or_default(),
            w.target_document_id.clone().unwrap_or_default(),
        );
        combos.entry(key).or_default().push(w.id.clone());
    }
    let dups: Vec<&(String, String, String)> = combos
        .iter()
        .filter(|(_, ids)| ids.iter().collect::<BTreeSet<_>>().len() > 1)
        .map(|(k, _)| k)
        .collect();
    if !dups.is_empty() {
        let detail = dups
            .iter()
            .take(5)
            .map(|(p, src, tgt)| {
                let short = p.rsplit('#').next().unwrap_or(p.as_str());
                format!("{short} {src}->{tgt}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        failures.push(format!("duplicate wires ({} combos): {detail}", dups.len()));
    }
    stats.wire_combos = Some(combos.len());

    // 4. legacy-namespace drift.
    for legacy in LEGACY_NAMESPACES {
        let n = triples
            .iter()
            .filter(|(_, p, _)| p.starts_with(legacy))
            .count();
        if n > 0 {
            failures.push(format!("namespace drift: {n} triples still in {legacy}"));
        }
    }

    // 5. rogue-predicate scan: primary-ns predicates outside the contract
    //    (known class predicates ∪ wire predicate_uris).
    let mut known = contract.known_predicate_uris();
    for w in contract.wires.values() {
        if let Some(uri) = &w.predicate_uri {
            known.insert(uri.clone());
        }
    }
    let seen_preds: BTreeSet<&str> = triples
        .iter()
        .map(|(_, p, _)| p.as_str())
        .filter(|p| p.starts_with(&wfns))
        .collect();
    let rogue: BTreeSet<&str> = seen_preds
        .into_iter()
        .filter(|p| !known.contains(*p))
        .collect();
    if !rogue.is_empty() {
        let mut locals: Vec<String> = rogue
            .iter()
            .map(|p| p.rsplit('#').next().unwrap_or(p).to_string())
            .collect();
        locals.sort();
        failures.push(format!(
            "predicates outside vocab (improvised?): {locals:?}"
        ));
    }

    // 7. managed-doc drift (non-fatal): live read-back vs recorded renderSha256.
    if let (Some(docs_md), Some(doc_provenance)) = (docs_md, doc_provenance) {
        let common: BTreeSet<&String> = docs_md
            .keys()
            .filter(|k| doc_provenance.contains_key(*k))
            .collect();
        for did in common {
            let recorded = doc_provenance
                .get(did)
                .and_then(|m| m.get("renderSha256"))
                .filter(|s| !s.is_empty());
            let Some(recorded) = recorded else {
                continue;
            };
            let live_md = &docs_md[did];
            if sha256_text(&canonical_md(live_md)) != *recorded {
                warnings.push(format!(
                    "doc {did}: live content drifted from recorded renderSha256 \
                     (hand-edited since last ingest)"
                ));
            }
        }
    }

    AssertReport {
        passed: failures.is_empty(),
        failures,
        warnings,
        stats,
    }
}

#[cfg(test)]
mod tests;
