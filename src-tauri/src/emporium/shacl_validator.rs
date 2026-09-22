//! Pre-store SHACL validation — the loud-halt gate the reconcile engine runs
//! over a `desired` graph BEFORE it writes (EA-1).
//!
//! The contract: a Meaningful-Objects reconcile projects a source to a `desired`
//! triple set, then a `diff_triples` against the store yields a minimal-delta
//! [`TripleDiff`]. This module is the validation FACE inserted between those two
//! steps: it derives SHACL shapes from the SAME vocab contract
//! ([`super::shacl_emit::vocab_to_shacl`]) and validates the `desired` triples
//! against them. A violation is a LOUD HALT — `Err("SHACL: …")`, no partial
//! write — matching the deliberate inversion the memory/document appliers
//! already use (and the `I1 NO MEMORY WITHOUT PROVENANCE` gate's prefixed-error
//! convention).
//!
//! The validation runs through the REAL rudof engine (`shacl_validation` native
//! mode over a `rudof_rdf` `InMemoryGraph`): no hand-rolled mirror of SHACL
//! semantics. The `desired` triples are serialized to N-Triples and parsed into
//! a throwaway in-memory graph (the validator never touches the production
//! oxigraph store — it validates the candidate state, so it cannot corrupt the
//! store on the way to deciding whether the write is legal).

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rudof_rdf::rdf_core::RDFFormat;
use rudof_rdf::rdf_impl::{InMemoryGraph, ReaderMode};
use shacl_ir::compiled::schema_ir::SchemaIR;
use shacl_validation::shacl_processor::{GraphValidation, ShaclProcessor, ShaclValidationMode};
use shacl_validation::store::{Graph, ShaclDataManager};

use crate::emporium::contract::VocabularyContract;
use crate::emporium::shacl_emit::vocab_to_shacl;
use crate::emporium::terms::{sha256_text, Triple};

/// Process-wide serialization of the rudof SHACL engine. rudof 0.2.12's
/// `ShaclDataManager::load` (shapes COMPILE) + the native `ShaclProcessor`
/// (VALIDATION) were observed as nondeterministically *missing* violations when
/// many validations run in parallel (the test suite under CPU load, or a busy
/// cell serving concurrent writes) — an empirical hazard the lock was added to fix.
///
/// SCOPE INVESTIGATION (S7, appraisal §4.4 item 4 — "is the lock per-compile or
/// per-validation?"): a source audit of the crates we actually exercise
/// (`shacl_validation` 0.2.12, `shacl_ir` 0.2.9, `rudof_rdf` 0.2.12, `iri_s` 0.2.9)
/// found NO unsynchronized process-global mutable state — every global is a
/// thread-safe `OnceLock` vocab-IRI cache; `do_validate` / `NativeEngine::new` /
/// `ValidationCache` are all per-call and read the `&SchemaIR` immutably. That is
/// evidence the validation step is likely safe, BUT a nondeterministic race can hide
/// in a transitive parser dependency, and the prior observation is authoritative:
/// per "honesty over heroics", we do NOT remove a lock guarding an observed hazard on
/// this hot path on the strength of a non-exhaustive audit. So the lock is RETAINED
/// around validation.
///
/// What the S7 COMPILE CACHE ([`SHAPES_SCHEMA_CACHE`]) changes: the expensive
/// `ShaclDataManager::load` (parse multi-KB Turtle shapes + build the SHACL AST +
/// compile the IR) — which used to run under this lock on EVERY validated write,
/// serializing that cost across ALL graphs — now runs at most once per shapes graph.
/// The lock is therefore held only for the (cheap) per-instance validation, not the
/// (expensive) compile, removing most of the cross-graph contention. Poison is
/// recovered — a panic mid-validation must not wedge every future write.
static SHACL_ENGINE_LOCK: Mutex<()> = Mutex::new(());

/// Compiled-shapes cache: `sha256(shapes TTL)` -> the compiled [`SchemaIR`]. Keyed on
/// the sha of the emitted shapes TTL, which is correct by construction: `vocab_to_shacl`
/// is deterministic, so a registered contract's sha ⟺ its shapes TTL ⟺ this key (and a
/// chamber-proposed ontology keys on the content hash of its own TTL — the task's
/// "content hash of their shapes TTL"). Simple + unbounded like `rdf_store_service`'s
/// store cache; the number of distinct shapes graphs in a process is the vocab count,
/// so it is effectively bounded. Poison-recovered.
static SHAPES_SCHEMA_CACHE: OnceLock<Mutex<BTreeMap<String, Arc<SchemaIR>>>> = OnceLock::new();

/// Count of ACTUAL `ShaclDataManager::load` compilations (cache MISSES) — observability
/// (a sibling of the appraisal's "op_count computed then discarded" wart) and the part-2
/// cache-hit test's observable.
static SHACL_COMPILE_COUNT: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
fn shacl_compile_count() -> u64 {
    SHACL_COMPILE_COUNT.load(Ordering::Relaxed)
}

/// Get the compiled [`SchemaIR`] for `shapes_ttl`, compiling + caching on the first miss.
/// Compilation touches the rudof engine, so a (rare, post-cache) miss compiles UNDER the
/// process-wide [`SHACL_ENGINE_LOCK`]; the cache lookup + insert take only the cache mutex
/// (never nested with the engine lock, so no deadlock). A concurrent double-miss harmlessly
/// compiles twice (idempotent — `SchemaIR` is a pure function of the TTL, last write wins).
fn compiled_schema(
    shapes_ttl: &str,
    contract_name: &str,
) -> Result<Arc<SchemaIR>, Vec<ViolationRecord>> {
    let key = sha256_text(shapes_ttl);
    let cache = SHAPES_SCHEMA_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(schema) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
    {
        return Ok(schema);
    }
    // MISS: compile under the engine lock (rudof's shapes-load path is serialized).
    let compiled = {
        let _engine_guard = SHACL_ENGINE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ShaclDataManager::load(
            &mut Cursor::new(shapes_ttl.as_bytes()),
            "sophia-shacl-shapes",
            RDFFormat::Turtle,
            None,
        )
        .map_err(|e| {
            vec![ViolationRecord {
                focus_node: contract_name.to_string(),
                shape: None,
                property_path: None,
                offending_value: None,
                message: format!("SHACL: failed to compile vocab-derived shapes: {e}"),
                severity: "Violation".to_string(),
            }]
        })?
    };
    SHACL_COMPILE_COUNT.fetch_add(1, Ordering::Relaxed);
    let schema = Arc::new(compiled);
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // BOUNDED: served packs are few, but RUNTIME chamber ontologies mint a fresh
    // shapes TTL (→ a fresh key) per proposed contract, so the keyspace is
    // unbounded. Cap with a coarse clear-on-full (correct over clever: a burst of
    // distinct chamber shapes past the cap just re-compiles; the map never grows
    // without bound). The cap sits far above any realistic served + live-chamber
    // working set, so steady-state hit rate is unaffected.
    const MAX_SCHEMA_CACHE: usize = 512;
    if guard.len() >= MAX_SCHEMA_CACHE && !guard.contains_key(&key) {
        guard.clear();
    }
    // FIRST-WRITER-WINS: a concurrent double-miss must not overwrite an already-cached
    // entry — the cached `Arc` for a key is immutable once set, so a later validation of
    // the same shapes always returns the SAME compilation (the loser's compile is
    // discarded). Keeps the cache pointer-stable under concurrency.
    let cached = Arc::clone(guard.entry(key).or_insert(schema));
    Ok(cached)
}

/// Serialize a `desired` triple set to N-Triples (one `<s> <p> o .` per line).
/// The object term already renders byte-faithful N-Triples via
/// [`crate::emporium::terms::Term::as_nt`] (the same serialization the appliers
/// feed `SparqlEvaluator`), so the validator sees EXACTLY the triples the store
/// would. Placeholder objects render as their reserved
/// `urn:wf-emit:placeholder:…` IRI — a NamedNode, valid N-Triples.
fn triples_to_ntriples(desired: &[Triple]) -> String {
    let mut out = String::new();
    for (s, p, o) in desired {
        out.push_str(&format!("<{s}> <{p}> {} .\n", o.as_nt()));
    }
    out
}

/// A single SHACL validation violation, extracted with structural detail (EA-6).
///
/// This is the agent-actionable + ledger-recordable form of a conformance
/// failure: it names WHICH subject (`focus_node`), the offending `property_path`
/// and `offending_value` (when the engine reports them), the failing `shape`, a
/// human `message`, and the SHACL `severity`. The String loud-halt path (the
/// EA-1 contract) is derived from a `Vec<ViolationRecord>` so the two faces never
/// diverge: the string is just these records joined.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ViolationRecord {
    /// The focus node (subject) that violates a constraint.
    pub(crate) focus_node: String,
    /// The source shape IRI that failed (e.g.
    /// `urn:sophia:shacl:sophia-memory-core#SourceReferenceShape`), when reported.
    pub(crate) shape: Option<String>,
    /// The property path (predicate URI) that violated a constraint, when the
    /// failing constraint is path-scoped (minCount/datatype/maxCount).
    pub(crate) property_path: Option<String>,
    /// The offending RDF value (the term that failed a datatype/nodeKind check),
    /// when the engine reports an `sh:value`.
    pub(crate) offending_value: Option<String>,
    /// Human-readable message from the shape, or a stable fallback.
    pub(crate) message: String,
    /// SHACL severity level ("Violation" | "Warning" | "Info").
    pub(crate) severity: String,
}

impl ViolationRecord {
    /// The single-line rendering used inside the String loud-halt path: the
    /// EA-1-stable `"{focus}: {message}"` shape (so existing callers and the
    /// journal see no behavioral change).
    fn to_halt_line(&self) -> String {
        format!("{}: {}", self.focus_node, self.message)
    }
}

/// COMPILE-CHECK a contract's derived SHACL shapes (EA-3 §4, the chamber publish
/// gate). Derives the shapes graph via [`vocab_to_shacl`] and loads it through the
/// SAME rudof `ShaclDataManager::load` path [`validate_desired_structured`] uses —
/// proving the agent's PROPOSED ontology yields a well-formed, loadable shapes
/// graph BEFORE it is stored. Returns the derived Turtle on success so the caller
/// can surface / cache it; `Err("SHACL: …")` if the derived shapes do not compile
/// (a structurally-broken proposal — e.g. a class whose predicate CURIE cannot
/// expand — is rejected loud at publish time, never silently stored).
///
/// This is the publish-time twin of the per-instance validation: instances are
/// validated against shapes at write time; the ontology itself is validated
/// (its shapes compile) at propose time.
pub(crate) fn compile_check_contract(contract: &VocabularyContract) -> Result<String, String> {
    let shapes_ttl = vocab_to_shacl(contract);
    ShaclDataManager::load(
        &mut Cursor::new(shapes_ttl.as_bytes()),
        "sophia-shacl-shapes",
        RDFFormat::Turtle,
        None,
    )
    .map_err(|e| {
        format!(
            "SHACL: proposed ontology '{}' derives shapes that do not compile: {e}",
            contract.name
        )
    })?;
    Ok(shapes_ttl)
}

/// Validate a `desired` triple set against the SHACL shapes derived from
/// `contract`, LOUD-HALTING on the first conformance failure.
///
/// Returns `Ok(())` iff the desired graph conforms. On violation, returns
/// `Err("SHACL: …")` derived from the SAME structured extraction
/// [`validate_desired_structured`] returns — the `SHACL:` prefix mirrors the
/// `I1 …` gate's prefixed-error convention. EA-1's call sites (reconcile) use
/// this string path unchanged.
///
/// An empty `desired` (a fully-reclaimed projection) trivially conforms.
pub(crate) fn validate_desired(
    desired: &[Triple],
    contract: &VocabularyContract,
) -> Result<(), String> {
    match validate_desired_structured(desired, contract) {
        Ok(()) => Ok(()),
        Err(violations) => Err(violations_to_halt_string(contract, &violations)),
    }
}

/// Render a `Vec<ViolationRecord>` into the EA-1-stable loud-halt String. Public
/// to the crate so the apply fork can build the same message when it carries the
/// structured violations forward (Halt policy).
pub(crate) fn violations_to_halt_string(
    contract: &VocabularyContract,
    violations: &[ViolationRecord],
) -> String {
    let details: Vec<String> = violations
        .iter()
        .map(ViolationRecord::to_halt_line)
        .collect();
    format!(
        "SHACL: {} violation(s) in desired graph for vocab '{}': {}",
        violations.len(),
        contract.name,
        details.join("; ")
    )
}

/// The STRUCTURED validation entry point (EA-6). Same real-rudof engine + same
/// vocab-derived shapes as [`validate_desired`], but returns the full
/// [`ViolationRecord`] list on failure (not a flattened string), so the caller
/// can (a) hand the agent an actionable, per-constraint report on Halt and
/// (b) record each violation to the violation ledger on FlagAndAccept.
///
/// Engine/parse/compile errors are surfaced as a SINGLE synthetic
/// [`ViolationRecord`] (focus = the contract, message prefixed `SHACL:`) so the
/// caller never has to thread a second error channel — an apparatus failure is a
/// loud violation, not a silent pass.
///
/// An empty `desired` conforms trivially (`Ok(())`) without invoking the engine.
///
/// This is the GRAPH-UNAWARE entry point: it runs the rudof structural engine plus
/// any `sh:select` constraints, loading the data into the COMMONS memory projection
/// graph for named-graph context. Callers that know the real per-observer membrane
/// graph (the memory write path) should use [`validate_desired_structured_in_graph`]
/// so membrane-scoped SELECTs (I1/I2b) see the correct perspective.
pub(crate) fn validate_desired_structured(
    desired: &[Triple],
    contract: &VocabularyContract,
) -> Result<(), Vec<ViolationRecord>> {
    // The neutral default context: the un-segmented commons projection graph. A
    // membrane-scoped SELECT (FILTER on `:agent:`) correctly does NOT fire here —
    // matching the legacy graph-unaware behavior for the materializer-reconcile
    // callers (whose vocab shapes carry no sh:sparql today).
    let commons = crate::rdf_authority::memory_projection_graph_iri("validate-default");
    validate_desired_structured_in_graph(desired, contract, &commons)
}

/// The GRAPH-AWARE structured validation entry point. Identical to
/// [`validate_desired_structured`] but loads the `desired` triples into
/// `target_graph_iri` (the per-observer membrane / projection graph the write
/// targets) so that GRAPH-scoped `sh:select` constraints — the membrane-witness
/// (I1), voice-needs-witness (I2b), etc. invariants that rudof's single flat
/// `InMemoryGraph` CANNOT evaluate — see the perspective the store will hold.
///
/// The two violation sources MERGE: rudof's structural violations first, then the
/// `sh:select` violations (a returned `$this` solution = one violation). The caller
/// applies ONE `ValidationPolicy` gate over the merged list, so the tier (Halt vs
/// FlagAndAccept) controls BOTH faces uniformly — nothing here hard-codes a verdict.
pub(crate) fn validate_desired_structured_in_graph(
    desired: &[Triple],
    contract: &VocabularyContract,
    target_graph_iri: &str,
) -> Result<(), Vec<ViolationRecord>> {
    // The shapes graph is the contract's derived SHACL (the single source of truth
    // for {survey-span, Lean, SHACL}). `validate_against_shapes` runs BOTH faces —
    // the rudof structural engine and the oxigraph sh:select evaluator — over it.
    validate_against_shapes(
        &vocab_to_shacl(contract),
        desired,
        target_graph_iri,
        &contract.name,
    )
}

/// Validate `desired` against an explicit SHACL `shapes_ttl`, running BOTH the
/// rudof structural engine AND the oxigraph `sh:select` evaluator and MERGING their
/// violations into one list. `target_graph_iri` is the named graph the data is
/// loaded under for the SPARQL face (membrane context); `contract_name` only labels
/// synthetic engine-fault violations. Factored out of
/// [`validate_desired_structured_in_graph`] so the merge is exercisable against a
/// shapes graph that carries `sh:sparql` constraints (the §3 agent ontology) — the
/// production `vocab_to_shacl` does not emit `sh:sparql` yet, so the live memory
/// path's SPARQL side is currently an (empty) no-op merge until those shapes are
/// emitted (mechanical follow-up), but the merge wiring is identical.
pub(crate) fn validate_against_shapes(
    shapes_ttl: &str,
    desired: &[Triple],
    target_graph_iri: &str,
    contract_name: &str,
) -> Result<(), Vec<ViolationRecord>> {
    if desired.is_empty() {
        return Ok(());
    }

    // A compile/parse/engine fault is itself a loud failure: wrap it as one
    // synthetic violation so the caller's single failure channel carries it.
    let engine_fault = |stage: &str, e: String| {
        vec![ViolationRecord {
            focus_node: contract_name.to_string(),
            shape: None,
            property_path: None,
            offending_value: None,
            message: format!("SHACL: {stage}: {e}"),
            severity: "Violation".to_string(),
        }]
    };

    // (0) The sh:select constraints carried by the shapes — evaluated over oxigraph
    //     (the capability rudof 0.2.9 LACKS: no SPARQL constraint component + no
    //     named-graph context). These MERGE with the structural violations below
    //     into ONE list the caller gates uniformly.
    let mut sparql_violations = crate::emporium::shacl_sparql::evaluate_sparql_constraints(
        shapes_ttl,
        &triples_to_ntriples(desired),
        target_graph_iri,
    )
    .map_err(|e| engine_fault("sh:select evaluation", e))?;

    // (1) The compiled shapes graph — from the S7 cache. The expensive
    // `ShaclDataManager::load` (parse + AST + IR compile) runs at most once per shapes
    // TTL (see `compiled_schema` / SHAPES_SCHEMA_CACHE), instead of on every write.
    let schema = compiled_schema(shapes_ttl, contract_name)?;

    // Serialize the rudof native ShaclProcessor + the InMemoryGraph parse from here
    // through report extraction: the native `ShaclProcessor` was observed to race under
    // concurrency (see SHACL_ENGINE_LOCK — RETAINED around validation, not disproven).
    // The guard is held to function end so the report is also extracted under it. The
    // oxigraph `sh:select` pass above does not need it (each call builds its own Store),
    // and the compile above already ran under the lock on its (rare) cache miss.
    let _engine_guard = SHACL_ENGINE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // (2) Parse the desired triples into a throwaway in-memory data graph.
    let data_nt = triples_to_ntriples(desired);
    let in_mem =
        InMemoryGraph::from_str(&data_nt, &RDFFormat::NTriples, None, &ReaderMode::default())
            .map_err(|e| {
                engine_fault(
                    "failed to parse desired graph for validation",
                    e.to_string(),
                )
            })?;
    let graph = Graph::from_graph(in_mem)
        .map_err(|e| engine_fault("failed to wrap data graph", e.to_string()))?;

    // (3) Validate via the REAL native engine over the InMemoryGraph.
    let mut validation = GraphValidation::from_graph(graph, ShaclValidationMode::Native);
    let report = ShaclProcessor::<InMemoryGraph>::validate(&mut validation, schema.as_ref())
        .map_err(|e| engine_fault("validation engine error", e.to_string()))?;

    // Conformant under BOTH the structural engine AND the sh:select constraints =
    // a clean pass. Only when rudof conforms AND no SPARQL constraint fired do we
    // return Ok — otherwise we fall through and merge both violation sources.
    if report.conforms() && sparql_violations.is_empty() {
        return Ok(());
    }

    // (4) Extract the structured violations from the rudof report, then APPEND the
    //     sh:select violations so the caller gates ONE merged list.
    let mut violations = Vec::new();
    for result in report.results() {
        // The property path: prefer the bare predicate URI of an `sh:path`
        // predicate path (the common case for our minCount/datatype/maxCount
        // shapes); fall back to the Display form for compound paths.
        let property_path = result.path().map(|p| match p.pred() {
            Some(iri) => iri.to_string(),
            None => p.to_string(),
        });
        violations.push(ViolationRecord {
            focus_node: result.focus_node().to_string(),
            shape: result.source().map(|s| s.to_string()),
            property_path,
            offending_value: result.value().map(|v| v.to_string()),
            message: result
                .message()
                .unwrap_or("constraint violated")
                .to_string(),
            severity: format!("{:?}", result.severity()),
        });
    }
    // Merge the sh:select violations (the rudof-invisible GRAPH-scoped + cross-
    // subject constraints) into the same list the caller gates + ledgers.
    violations.append(&mut sparql_violations);
    Err(violations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::{Literal, NamedNode};

    use crate::emporium::contract::memory_core_vocabulary;
    use crate::emporium::terms::Term;

    const MEM: &str = "http://mnemosyne.dev/memory#";
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

    fn uri(s: &str) -> Term {
        Term::Uri(NamedNode::new(s).expect("valid IRI"))
    }
    fn string(s: &str) -> Term {
        Term::Lit(Literal::new_simple_literal(s))
    }
    fn typed(value: &str, dt: &str) -> Term {
        Term::Lit(Literal::new_typed_literal(
            value,
            NamedNode::new(format!("{XSD}{dt}")).unwrap(),
        ))
    }

    /// A WELL-FORMED SourceReference: rdf:type + the one required predicate
    /// (mem:sourceKind, a string). The simplest fully-conformant memory subject.
    fn well_formed_source_ref() -> Vec<Triple> {
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:src:abc";
        vec![
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
            (
                s.to_string(),
                format!("{MEM}sourceKind"),
                string("DocumentBlock"),
            ),
        ]
    }

    /// (a) WELL-FORMED desired graph VALIDATES — the seam is not a tarpit; a
    /// conformant projection passes cleanly through the real engine.
    #[test]
    fn well_formed_desired_conforms() {
        let desired = well_formed_source_ref();
        let result = validate_desired(&desired, memory_core_vocabulary());
        assert!(
            result.is_ok(),
            "well-formed SourceReference must conform: {result:?}"
        );
    }

    /// (b) TEETH-CHECK: a SourceReference MISSING its required mem:sourceKind is
    /// REJECTED (sh:minCount 1). Proves the seam actually enforces — not a
    /// tautology that passes everything.
    #[test]
    fn missing_required_predicate_is_rejected() {
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:src:bad";
        let desired = vec![
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
            // mem:sourceKind (required) deliberately OMITTED.
        ];
        let result = validate_desired(&desired, memory_core_vocabulary());
        assert!(
            result.is_err(),
            "a SourceReference without its required mem:sourceKind must be rejected"
        );
        let msg = result.unwrap_err();
        assert!(msg.starts_with("SHACL:"), "loud-halt prefix present: {msg}");
    }

    /// (c) TEETH-CHECK #2: a CLOSED-shape violation — a predicate OUTSIDE the
    /// contract on an otherwise-valid subject is rejected (sh:closed true). This
    /// is the SHACL image of the frozen-vocab guard.
    #[test]
    fn rogue_predicate_outside_contract_is_rejected() {
        let mut desired = well_formed_source_ref();
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:src:abc";
        desired.push((
            s.to_string(),
            "http://example.org/not-in-the-contract".to_string(),
            string("rogue"),
        ));
        let result = validate_desired(&desired, memory_core_vocabulary());
        assert!(
            result.is_err(),
            "a predicate outside the closed contract must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// (d) TEETH-CHECK #3: a DATATYPE violation — mem:sourceKind carries a
    /// xsd:integer where the contract declares xsd:string (sh:datatype).
    #[test]
    fn wrong_datatype_is_rejected() {
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:src:dt";
        let desired = vec![
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
            (
                s.to_string(),
                format!("{MEM}sourceKind"),
                typed("42", "integer"), // declared xsd:string — wrong type
            ),
        ];
        let result = validate_desired(&desired, memory_core_vocabulary());
        assert!(
            result.is_err(),
            "a xsd:integer where xsd:string is required must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }

    /// An empty desired (fully-reclaimed projection) trivially conforms without
    /// invoking the engine — the converged-reconcile cheap path.
    #[test]
    fn empty_desired_conforms_trivially() {
        assert!(validate_desired(&[], memory_core_vocabulary()).is_ok());
    }

    /// EA-3 §4: a real, well-formed contract's derived shapes COMPILE (the chamber
    /// publish gate accepts a valid proposal and returns the derived Turtle).
    #[test]
    fn compile_check_accepts_a_well_formed_contract() {
        let ttl = compile_check_contract(memory_core_vocabulary())
            .expect("a well-formed contract's shapes must compile");
        assert!(
            ttl.contains("a sh:NodeShape"),
            "derived shapes were returned"
        );
    }

    #[test]
    fn compile_check_accepts_workflow_ui_contract() {
        let contract = crate::emporium::contract::get_vocabulary("workflow-ui")
            .expect("workflow-ui registered");
        let ttl =
            compile_check_contract(contract).expect("workflow-ui derived shapes must compile");
        assert!(ttl.contains("workflow-ui#PageTurnDecisionShape"));
        assert!(ttl.contains("workflow-ui#WorkflowAdventurePacketShape"));
        assert!(ttl.contains("workflow-ui#NavigationRouteShape"));
        assert!(ttl.contains("recommendedRouteId"));
        assert!(ttl.contains("followedRouteId"));
        assert!(ttl.contains("rawSparqlJson"));
    }

    // ── S7 PART 2: the compiled-shapes cache ───────────────────────────────

    /// Two validations of the SAME contract compile its shapes only ONCE — the second
    /// reuses the cached compilation (a recompile would insert a fresh `Arc<SchemaIR>`,
    /// changing the pointer). Proven over the REAL rudof compile path (no mocks): the
    /// compiled schema is cached under the shapes-TTL sha, and the cached `Arc` is
    /// pointer-stable across a repeat validation. Parallel-safe (keyed by this contract's
    /// sha; a cache HIT never inserts, so the pointer cannot move under a concurrent
    /// same-key validation).
    #[test]
    fn repeated_validation_of_a_contract_reuses_the_compiled_shapes_cache() {
        use std::sync::Arc;
        let contract = memory_core_vocabulary();
        let desired = well_formed_source_ref();
        let key = crate::emporium::terms::sha256_text(
            &crate::emporium::shacl_emit::vocab_to_shacl(contract),
        );

        // Warm the cache: after a validation the compiled SchemaIR is cached under the sha.
        validate_desired(&desired, contract).expect("well-formed conforms");
        assert!(
            shacl_compile_count() >= 1,
            "at least one real ShaclDataManager::load compile has happened + been counted"
        );
        let cached_after_first = {
            let cache = SHAPES_SCHEMA_CACHE
                .get()
                .expect("cache initialized after a validation")
                .lock()
                .unwrap();
            Arc::clone(
                cache
                    .get(&key)
                    .expect("compiled shapes are cached under the shapes-TTL sha key"),
            )
        };

        // A second validation of the same contract must REUSE the cached compilation.
        validate_desired(&desired, contract).expect("well-formed conforms again");
        let cached_after_second = {
            let cache = SHAPES_SCHEMA_CACHE.get().expect("cache").lock().unwrap();
            Arc::clone(cache.get(&key).expect("still cached"))
        };
        assert!(
            Arc::ptr_eq(&cached_after_first, &cached_after_second),
            "the second validation reused the cached compiled shapes (no recompile)"
        );
    }

    // ── EA-6: STRUCTURED violation extraction ──────────────────────────────

    /// A well-formed graph conforms under the structured path too (no violations).
    #[test]
    fn structured_well_formed_conforms() {
        let desired = well_formed_source_ref();
        assert!(validate_desired_structured(&desired, memory_core_vocabulary()).is_ok());
    }

    /// The teeth-check, STRUCTURED: a SourceReference missing its required
    /// mem:sourceKind yields a ViolationRecord that names the focus node and the
    /// offending property path (mem:sourceKind) — agent-actionable, not a string.
    #[test]
    fn structured_missing_required_names_focus_and_path() {
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:src:bad";
        let desired = vec![
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
            // mem:sourceKind (required) deliberately OMITTED.
        ];
        let violations = validate_desired_structured(&desired, memory_core_vocabulary())
            .expect_err("a missing required predicate must produce a structured violation");
        assert!(!violations.is_empty(), "at least one structured violation");
        let v = &violations[0];
        assert_eq!(v.focus_node, s, "violation names the focus subject");
        // minCount violations are path-scoped → the path names mem:sourceKind.
        assert_eq!(
            v.property_path.as_deref(),
            Some("http://mnemosyne.dev/memory#sourceKind"),
            "the violated property path is the missing required predicate"
        );
        assert_eq!(
            v.severity, "Violation",
            "default SHACL severity is Violation"
        );
        // The derived String path is byte-identical in spirit (SHACL: prefix +
        // focus:message lines) — proving the two faces never diverge.
        let s_form = violations_to_halt_string(memory_core_vocabulary(), &violations);
        assert!(s_form.starts_with("SHACL:"));
        assert!(s_form.contains(s), "the halt string carries the focus node");
    }

    /// A datatype violation, STRUCTURED: the offending VALUE is surfaced (the
    /// xsd:integer "42" that should have been an xsd:string). This is the field
    /// the ledger records so a reviewer sees WHAT was wrong, not just where.
    #[test]
    fn structured_datatype_violation_surfaces_offending_value() {
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:src:dt";
        let desired = vec![
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
            (
                s.to_string(),
                format!("{MEM}sourceKind"),
                typed("42", "integer"),
            ),
        ];
        let violations = validate_desired_structured(&desired, memory_core_vocabulary())
            .expect_err("a datatype mismatch must produce a structured violation");
        // Find the datatype violation (path = mem:sourceKind, value reported).
        let dt = violations
            .iter()
            .find(|v| v.property_path.as_deref() == Some("http://mnemosyne.dev/memory#sourceKind"))
            .expect("a violation scoped to mem:sourceKind");
        assert!(
            dt.offending_value
                .as_deref()
                .map(|v| v.contains("42"))
                .unwrap_or(false),
            "the offending value (\"42\") is surfaced for the reviewer: {:?}",
            dt.offending_value
        );
    }

    // ── sh:select INTEGRATION: the graph-aware merge path ──────────────────

    /// The graph-aware variant preserves the STRUCTURAL engine's verdict: a missing
    /// required predicate is still rejected, and the graph IRI threads through
    /// without disturbing rudof's result (the merge appends, it does not replace).
    #[test]
    fn graph_aware_variant_preserves_structural_violations() {
        let s = "urn:mnemosyne:local:graph:lab:projection:memory:agent:alice:src:bad";
        let desired = vec![
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
            // mem:sourceKind (required) deliberately OMITTED.
        ];
        let membrane = "urn:mnemosyne:local:graph:lab:projection:memory:agent:alice";
        let violations =
            validate_desired_structured_in_graph(&desired, memory_core_vocabulary(), membrane)
                .expect_err(
                "a missing required predicate must still be rejected under the graph-aware path",
            );
        assert!(
            violations
                .iter()
                .any(|v| v.property_path.as_deref()
                    == Some("http://mnemosyne.dev/memory#sourceKind")),
            "the structural minCount violation survives the sh:select merge: {violations:?}"
        );
    }

    /// A well-formed graph conforms under the graph-aware path too (the merge of an
    /// empty sh:select-violation set with an empty structural set is still Ok).
    #[test]
    fn graph_aware_well_formed_conforms() {
        let desired = well_formed_source_ref();
        let membrane = "urn:mnemosyne:local:graph:lab:projection:memory:agent:alice";
        assert!(
            validate_desired_structured_in_graph(&desired, memory_core_vocabulary(), membrane)
                .is_ok(),
            "a well-formed projection conforms under the graph-aware merge path"
        );
    }

    /// THE MERGE, END-TO-END: a shapes graph carrying BOTH a structural shape
    /// (sh:minCount) AND a real §3 `sh:sparql` membrane constraint (I1) validates a
    /// record that violates BOTH — and `validate_against_shapes` returns them in ONE
    /// merged `Vec<ViolationRecord>`, the SAME list the `ValidationPolicy` gate +
    /// violation ledger consume. This is the integration the live memory path runs
    /// once `vocab_to_shacl` emits the §3 sh:sparql shapes (mechanical follow-up).
    #[test]
    fn merge_combines_structural_and_sparql_violations_in_one_list() {
        // Shapes: a closed structural shape requiring mem:sourceKind (minCount 1)
        // PLUS the real I1 membrane sh:sparql constraint (observedBy required inside
        // a …:agent: membrane). Both target the SAME record subject below.
        let shapes = r#"
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix agt:  <http://mnemosyne.dev/agent#> .
@prefix mem:  <http://mnemosyne.dev/memory#> .

agt:StructuralKindShape a sh:NodeShape ;
    sh:targetClass mem:MemoryRecord ;
    sh:property [ sh:path mem:sourceKind ; sh:minCount 1 ;
                  sh:message "structural: mem:sourceKind is required" ] .

agt:I1_MembraneWitnessShape a sh:NodeShape ;
    sh:targetClass mem:MemoryRecord ;
    sh:sparql [
        sh:severity sh:Violation ;
        sh:message "I1: a membrane record MUST be observedBy an agt:Agent." ;
        sh:prefixes [ sh:declare [ sh:prefix "mem" ; sh:namespace "http://mnemosyne.dev/memory#" ] ] ;
        sh:select """
            SELECT $this WHERE {
              GRAPH ?g { $this a mem:MemoryRecord . }
              FILTER( CONTAINS(STR(?g), ':projection:memory:agent:') )
              FILTER NOT EXISTS { GRAPH ?g { $this mem:observedBy ?obs . } }
            }
        """ ;
    ] .
"#;
        // A MemoryRecord with NO sourceKind (structural fail) AND no observedBy
        // (I1 sh:sparql fail), in an :agent: membrane graph.
        let s = "urn:rec:doubly-bad";
        let desired = vec![(
            s.to_string(),
            RDF_TYPE.to_string(),
            uri(&format!("{MEM}MemoryRecord")),
        )];
        let membrane = "urn:mnemosyne:local:graph:lab:projection:memory:agent:alice";

        let violations = validate_against_shapes(shapes, &desired, membrane, "merge-test")
            .expect_err("a doubly-malformed record must produce violations");

        // The STRUCTURAL violation (minCount on mem:sourceKind) is present…
        assert!(
            violations
                .iter()
                .any(|v| v.property_path.as_deref()
                    == Some("http://mnemosyne.dev/memory#sourceKind")),
            "rudof structural minCount violation present in the merged list: {violations:?}"
        );
        // …AND the sh:sparql I1 membrane violation (which rudof CANNOT produce) is
        // present in the SAME list, attributed to the I1 shape.
        assert!(
            violations.iter().any(|v| v.shape.as_deref()
                == Some("http://mnemosyne.dev/agent#I1_MembraneWitnessShape")
                && v.focus_node == s),
            "the sh:select I1 membrane violation is merged into the same list: {violations:?}"
        );
        assert!(
            violations.len() >= 2,
            "both faces contribute: {} violations",
            violations.len()
        );
    }

    // ── CA-1: the §3 shapes fire through the LIVE contract → vocab_to_shacl path ──

    use crate::emporium::contract::get_vocabulary;

    const MEM_RECORD: &str = "http://mnemosyne.dev/memory#MemoryRecord";
    const I1_SHAPE: &str = "http://mnemosyne.dev/agent#I1_MembraneWitnessShape";

    /// A bare MemoryRecord subject (rdf:type only) as a desired-insert triple set —
    /// the witnessless record the I1 membrane invariant must catch inside a membrane.
    fn witnessless_record(subject: &str) -> Vec<Triple> {
        vec![(subject.to_string(), RDF_TYPE.to_string(), uri(MEM_RECORD))]
    }

    /// TEETH (no-mock, live path): a witnessless MemoryRecord inside a per-observer
    /// `…:projection:memory:agent:` membrane VIOLATES the §3 I1 invariant — proven
    /// through `validate_desired_structured_in_graph` over the REAL sophia-agent-core
    /// contract, whose `vocab_to_shacl` now EMITS the raw §3 sh:select shapes. This is
    /// the gap this step closes: before the emit-wiring, this record passed silently.
    #[test]
    fn agent_core_i1_fires_on_a_witnessless_membrane_record_via_the_live_path() {
        let c = get_vocabulary("sophia-agent-core").expect("agent-core registered");
        let membrane = "urn:mnemosyne:local:graph:lab:projection:memory:agent:agent-deadbeef";
        let violations =
            validate_desired_structured_in_graph(&witnessless_record("urn:rec:naked"), c, membrane)
                .expect_err("a witnessless membrane record must trip I1 through the live path");
        assert!(
            violations
                .iter()
                .any(|v| v.shape.as_deref() == Some(I1_SHAPE) && v.focus_node == "urn:rec:naked"),
            "the §3 I1 sh:select fired via vocab_to_shacl: {violations:?}"
        );
    }

    /// CONFORMANT (live path): the SAME record WITH a `mem:observedBy` witness inside
    /// the membrane produces NO I1 violation — the wired §3 shape is not a tarpit.
    #[test]
    fn agent_core_i1_passes_a_witnessed_membrane_record_via_the_live_path() {
        let c = get_vocabulary("sophia-agent-core").expect("agent-core registered");
        let membrane = "urn:mnemosyne:local:graph:lab:projection:memory:agent:agent-deadbeef";
        let mut desired = witnessless_record("urn:rec:ok");
        desired.push((
            "urn:rec:ok".to_string(),
            "http://mnemosyne.dev/memory#observedBy".to_string(),
            uri("urn:sophia:agent:agent-deadbeef"),
        ));
        // No I1 violation (a witnessed membrane record conforms to the firm boundary).
        match validate_desired_structured_in_graph(&desired, c, membrane) {
            Ok(()) => {} // fully clean
            Err(violations) => assert!(
                !violations
                    .iter()
                    .any(|v| v.shape.as_deref() == Some(I1_SHAPE)),
                "a witnessed membrane record must NOT trip I1: {violations:?}"
            ),
        }
    }

    /// REGRESSION GUARD (the must-fix #1 commons exemption, live path): the SAME
    /// witnessless record in the UN-SEGMENTED commons graph must NOT trip I1 —
    /// `mem:observedBy` is optional at L0, and the membrane scope is the FILTER on
    /// the `:agent:` segment. Proven through the live contract path, not the raw shape.
    #[test]
    fn agent_core_i1_does_not_fire_on_a_commons_record_via_the_live_path() {
        let c = get_vocabulary("sophia-agent-core").expect("agent-core registered");
        let commons = "urn:mnemosyne:local:graph:lab:projection:memory";
        // The commons record has rdf:type only — same as the membrane teeth case.
        match validate_desired_structured_in_graph(
            &witnessless_record("urn:rec:commons"),
            c,
            commons,
        ) {
            Ok(()) => {} // clean — no I1, no other §3 shape fires on a bare commons record
            Err(violations) => assert!(
                !violations.iter().any(|v| v.shape.as_deref() == Some(I1_SHAPE)),
                "a commons record (un-segmented graph, no witness) MUST NOT trip I1: {violations:?}"
            ),
        }
    }

    /// THE MERGE on the LIVE contract path: an `agt:Agent` subject that violates BOTH
    /// a DERIVED structural shape (the closed agt:Agent shape — a rogue predicate
    /// outside the contract) AND a §3 `sh:select` invariant (IdentityUnification — the
    /// subject IRI ≠ `urn:sophia:agent:{agentId}`) yields BOTH violations in ONE merged
    /// list through `validate_desired_structured_in_graph` over the real agent-core
    /// contract. The structural rudof face and the oxigraph sh:select face merge under
    /// ONE policy. (Memory-core has no raw shapes; only agent-core exercises both faces
    /// from one contract.)
    #[test]
    fn agent_core_merges_a_structural_and_a_sparql_violation_in_one_list() {
        let c = get_vocabulary("sophia-agent-core").expect("agent-core registered");
        let agt = "http://mnemosyne.dev/agent#";
        // Subject IRI WRONG (the IdentityUnification §3 sh:select wants
        // urn:sophia:agent:{agentId}) … AND a rogue predicate (closed-shape structural
        // fail on the derived agt:Agent shape).
        let s = "urn:sophia:agent:WRONG-STEM";
        let identity_shape = "http://mnemosyne.dev/agent#IdentityUnificationShape";
        let desired = vec![
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri(&format!("{agt}Agent")),
            ),
            (
                s.to_string(),
                format!("{agt}agentId"),
                string("agent-cafef00d"),
            ),
            (
                s.to_string(),
                format!("{agt}voicing"),
                uri(&format!("{agt}Monovocal")),
            ),
            (
                s.to_string(),
                "http://example.org/not-in-the-contract".to_string(),
                string("rogue"),
            ),
        ];
        // The IdentityUnification SELECT runs unscoped (over the union), so the
        // membrane IRI is immaterial; use the commons for a neutral context.
        let commons = "urn:mnemosyne:local:graph:lab:projection:memory";
        let violations = validate_desired_structured_in_graph(&desired, c, commons)
            .expect_err("a doubly-malformed agent must produce violations");
        // The sh:select IdentityUnification face fired (rudof CANNOT produce this) …
        assert!(
            violations
                .iter()
                .any(|v| v.shape.as_deref() == Some(identity_shape) && v.focus_node == s),
            "the §3 IdentityUnification sh:select violation is in the merged list: {violations:?}"
        );
        // … AND a structural (rudof) violation is present in the SAME list (the rogue
        // predicate trips the derived closed agt:Agent shape).
        assert!(
            violations
                .iter()
                .any(|v| v.shape.as_deref() != Some(identity_shape)),
            "a structural rudof violation merged alongside the sh:select one: {violations:?}"
        );
        assert!(
            violations.len() >= 2,
            "both faces contribute: {} violations",
            violations.len()
        );
    }

    /// CONFORMANT under the merged path: the same shapes, but a record that
    /// satisfies BOTH (has sourceKind AND observedBy) yields NO violations — the
    /// merge is not a tarpit, both faces pass.
    #[test]
    fn merge_conforms_when_both_faces_pass() {
        let shapes = r#"
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix agt:  <http://mnemosyne.dev/agent#> .
@prefix mem:  <http://mnemosyne.dev/memory#> .

agt:StructuralKindShape a sh:NodeShape ;
    sh:targetClass mem:MemoryRecord ;
    sh:property [ sh:path mem:sourceKind ; sh:minCount 1 ] .

agt:I1_MembraneWitnessShape a sh:NodeShape ;
    sh:targetClass mem:MemoryRecord ;
    sh:sparql [
        sh:severity sh:Violation ;
        sh:message "I1: a membrane record MUST be observedBy an agt:Agent." ;
        sh:prefixes [ sh:declare [ sh:prefix "mem" ; sh:namespace "http://mnemosyne.dev/memory#" ] ] ;
        sh:select """
            SELECT $this WHERE {
              GRAPH ?g { $this a mem:MemoryRecord . }
              FILTER( CONTAINS(STR(?g), ':projection:memory:agent:') )
              FILTER NOT EXISTS { GRAPH ?g { $this mem:observedBy ?obs . } }
            }
        """ ;
    ] .
"#;
        let s = "urn:rec:good";
        let desired = vec![
            (
                s.to_string(),
                RDF_TYPE.to_string(),
                uri(&format!("{MEM}MemoryRecord")),
            ),
            (
                s.to_string(),
                format!("{MEM}sourceKind"),
                string("DocumentBlock"),
            ),
            (
                s.to_string(),
                format!("{MEM}observedBy"),
                uri("urn:sophia:agent:alice"),
            ),
        ];
        let membrane = "urn:mnemosyne:local:graph:lab:projection:memory:agent:alice";
        assert!(
            validate_against_shapes(shapes, &desired, membrane, "merge-test").is_ok(),
            "a record satisfying both the structural shape and the I1 sh:sparql shape conforms"
        );
    }
}
