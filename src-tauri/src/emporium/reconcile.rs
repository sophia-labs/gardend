//! `reconcile_class` — the GENERAL class-reconcile primitive.
//!
//! The four remaining Meaningful-Object kinds (salience → song → workspace[+wire])
//! are ONE shape: the store-context-dependent, type-keyed **class** span the Lean
//! P3a work formalized (`inScopeClass cur cls t := mem (t.1, rdfType, cls) cur`).
//! This module builds that primitive once; each kind is a thin `ClassScope` +
//! `<kind>_desired` instantiation.
//!
//! Body = the proven skeleton (mirrors `reconcile_graph_record`):
//! [`survey_class`] → `diff_triples` → `if is_empty return` (convergence = zero
//! SPARQL) → [`apply_diff`]. **Factored** into [`class_diff`] (survey+diff, no
//! apply) + [`apply_diff`] so workspace can later survey-all-classes-then-apply-
//! once. Returns the structured [`TripleDiff`] it applied — a converged reconcile
//! returns an empty diff; an edit returns only the changed slots.
//!
//! Mirrors the Lean class algebra (`Workspace.lean`): `mem_equiv_class` (the
//! `hsame: curStore = cur` survey-then-apply premise), `class_ops_dominate`,
//! `class_converged_zero_ops`.

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

use crate::emporium::contract::VocabularyContract;
use crate::emporium::shacl_validator::validate_desired;
use crate::emporium::survey::parse_term;
use crate::emporium::terms::{diff_triples, render_updates, Triple, TripleDiff};
use crate::rdf::sparql_string_literal;
use crate::runtime_config::RDF_TYPE;

/// Where the class span lives. As of the Layer-1 finish-flip EVERY reconciled
/// kind (salience / song / wire / workspace) is `Named` — each projects into its
/// own `:projection:*` named graph (the historical default-graph WART is FIXED).
/// [`Placement::Default`] is retained as the HONEST bare-graph option for any
/// future default-graph kind (`wrap_pattern` / `run_update` / `same_placement`
/// all handle it), currently unconstructed.
#[derive(Debug, Clone)]
pub(crate) enum Placement {
    /// `GRAPH <iri> { … }` — the survey/apply wrap in this named graph. ALL
    /// current reconciled kinds use this (each owns a `:projection:*` graph).
    Named(String),
    /// No `GRAPH` clause — bare default-graph survey/apply. Unconstructed now (the
    /// salience/song WART was migrated to named graphs); kept for honesty + any
    /// future default-graph kind.
    #[allow(dead_code)]
    Default,
}

/// How the class span is keyed onto its subjects. Per the 2026-06-21 ruling
/// (`Present` DROPPED — song matches-the-proof as a union of `Fixed` spans), this
/// is `Fixed`-only: one uniform span shape, every span reading straight onto a
/// Lean class (`inScopeClass cur cls`).
#[derive(Debug, Clone)]
pub(crate) enum SpanKey {
    /// Subjects that are `a <rdf_type>` (the type-keyed class head).
    Fixed { rdf_type: String },
}

/// The full scope of a class span: where it lives ([`Placement`]), how it is
/// keyed ([`SpanKey`]), and an optional single-cell-constancy `graphId` conjunct
/// for the default-graph kinds (salience filters `mnemo:graphId "gid"` so the
/// in-store survey/DELETE only reclaims THIS cell's valuations).
#[derive(Debug, Clone)]
pub(crate) struct ClassScope {
    pub(crate) placement: Placement,
    pub(crate) key: SpanKey,
    /// `Some((predicate_iri, value))` → AND a `<pred> "value"` filter onto each
    /// in-scope subject (the salience `mnemo:graphId` conjunct). The pair carries
    /// the predicate so the primitive is not salience-specific.
    pub(crate) graph_id_conjunct: Option<(String, String)>,
    /// `Some(set)` → restrict the span to these subjects (the class span ∩ the
    /// set): the SUBJECT-SCOPED UPSERT the generic ingest defaults to — a
    /// partial batch reconciles only its own subjects, class siblings never
    /// enter the survey. `None` → the whole class span (the derived-projection
    /// kinds, whose desired sets ARE the entire recomputed class, and the
    /// explicit `replace_class` ingest). `Some(∅)` surveys nothing.
    pub(crate) subjects: Option<std::collections::BTreeSet<String>>,
}

/// The body GRAPH-wrap for a [`Placement`]: `Named` wraps the body in
/// `GRAPH <iri> { … }`; `Default` is bare (the WART). Used by both the survey
/// (around the WHERE pattern) and [`apply_diff`] (around each rendered DATA body).
fn wrap_pattern(placement: &Placement, pattern: &str) -> String {
    match placement {
        Placement::Named(iri) => format!("GRAPH <{iri}> {{\n{pattern}\n}}"),
        Placement::Default => pattern.to_string(),
    }
}

/// Survey the class span: the Rust image of `faceOfClass cur cls`. Builds the
/// SELECT that matches the kind's wholesale DELETE span EXACTLY — for salience
/// that is the default-graph `?value a mnemo:BlockValuation ; mnemo:graphId "gid"
/// ; ?p ?o`. Returns every `(?s, ?p, parse_term(?o))` of every in-scope subject,
/// through the proven oxigraph-`term.to_string()` → [`parse_term`] round-trip
/// (so the surveyed `current` serializes identically to the bridged `desired`).
pub(crate) fn survey_class(store: &Store, scope: &ClassScope) -> Result<Vec<Triple>, String> {
    let SpanKey::Fixed { rdf_type } = &scope.key;
    // The class-head + optional graphId conjunct, then the free `?p ?o` over the
    // same subject — byte-for-byte the wholesale DELETE's WHERE pattern.
    // The conjunct is a predicate-object CONTINUATION of the `?s a <type>` head
    // (the `;` already binds the subject to `?s`), so it must NOT repeat `?s` —
    // byte-for-byte the wholesale DELETE's `?value a mnemo:BlockValuation ;
    // mnemo:graphId "gid" ; ?p ?o` shape, where the continuation lines carry only
    // `<pred> object`. (Repeating `?s` makes `?s` a variable PREDICATE → SPARQL
    // parse error "expected OPTIONAL".)
    let conjunct = match &scope.graph_id_conjunct {
        Some((pred, value)) => format!(
            "      <{pred}> {literal} ;\n",
            literal = sparql_string_literal(value),
        ),
        None => String::new(),
    };
    // Subject restriction (subject-scoped upsert): a `VALUES ?s { … }` clause
    // intersects the class span with the batch's subjects, so the diff's
    // `removes = current − desired` can only ever touch subjects the batch
    // actually restates — class siblings never enter `current`. An EMPTY
    // restriction surveys nothing (guarded here: an empty `VALUES {}` group is
    // a parse hazard, and "no subjects" must never widen to "all subjects").
    let values = match &scope.subjects {
        Some(subjects) if subjects.is_empty() => return Ok(Vec::new()),
        Some(subjects) => {
            let list = subjects
                .iter()
                .map(|s| format!("<{s}>"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("    VALUES ?s {{ {list} }}\n")
        }
        None => String::new(),
    };
    let pattern = format!("{values}    ?s <{RDF_TYPE}> <{rdf_type}> ;\n{conjunct}      ?p ?o .",);
    let query = format!(
        "SELECT ?s ?p ?o WHERE {{\n{}\n}}",
        wrap_pattern(&scope.placement, &pattern),
    );

    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| format!("parse class survey: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute class survey: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("class survey expected SELECT solutions".to_string()),
    };

    let mut out = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(|e| format!("class survey row: {e}"))?;
        let s = match sol.get("s").ok_or("class survey row missing ?s")? {
            oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
            other => return Err(format!("expected a NamedNode subject, got {other}")),
        };
        let p = match sol.get("p").ok_or("class survey row missing ?p")? {
            oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
            other => return Err(format!("expected a NamedNode predicate, got {other}")),
        };
        let o = parse_term(
            &sol.get("o")
                .ok_or("class survey row missing ?o")?
                .to_string(),
        );
        out.push((s, p, o));
    }
    Ok(out)
}

/// [`survey_class`] then `diff_triples`, NO apply. The factored half that lets a
/// caller survey-all-classes first and apply once (workspace). `current` is the
/// in-store span; `desired` is the bridged projection. Identical-by-value sets →
/// an empty [`TripleDiff`] (convergence = zero ops).
pub(crate) fn class_diff(
    store: &Store,
    scope: &ClassScope,
    desired: &[Triple],
) -> Result<TripleDiff, String> {
    let current = survey_class(store, scope)?;
    Ok(diff_triples(&current, desired))
}

/// Apply a [`TripleDiff`] direct-on-store, wrapped per [`Placement`]: DELETE the
/// `removes`, then INSERT the `adds` (the materializer's order). `Named` wraps
/// each rendered DATA body in `GRAPH <iri> { … }`; `Default` adds NOTHING (the
/// salience default-graph WART). `render_updates` chunks at 60 (one statement per
/// chunk — an incremental edit is a handful of triples = ONE chunk = atomic).
pub(crate) fn apply_diff(
    store: &Store,
    placement: &Placement,
    diff: &TripleDiff,
) -> Result<(), String> {
    // Detection surface: op counts are the algebra's convergence observable
    // (zero on a steady-state save). Emit them instead of discarding them, so
    // an operator can see non-convergence (a face drifting out of its lane, a
    // desired-set churning per save) without instrumenting anything.
    if !diff.adds.is_empty() || !diff.removes.is_empty() {
        log::info!(
            "reconcile_apply placement={} adds={} removes={}",
            match placement {
                Placement::Named(iri) => iri.as_str(),
                Placement::Default => "(default-graph)",
            },
            diff.adds.len(),
            diff.removes.len(),
        );
    }
    for body in render_updates("DELETE DATA", &diff.removes, 60) {
        run_update(store, placement, &body)?;
    }
    for body in render_updates("INSERT DATA", &diff.adds, 60) {
        run_update(store, placement, &body)?;
    }
    Ok(())
}

/// Apply one already-rendered `INSERT DATA` / `DELETE DATA` body, GRAPH-wrapping
/// it per [`Placement`] (`Named` → `GRAPH <iri> { body }`; `Default` → the body
/// verbatim). DIRECT-ON-STORE; bypasses the authority gate (like the wholesale
/// materializers).
fn run_update(store: &Store, placement: &Placement, body: &str) -> Result<(), String> {
    let wrapped = match placement {
        Placement::Named(iri) => wrap_data_body(body, iri)?,
        Placement::Default => body.to_string(),
    };
    SparqlEvaluator::new()
        .parse_update(&wrapped)
        .map_err(|e| format!("parse class update: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute class update: {e}"))?;
    // A live false-CLEAN hole until 2026-07-28: this is the one update path
    // that drives the store directly instead of going through
    // `rdf_query_service`, which marks at its own `:359`. Without this an
    // omphalos write was invisible to the flush's dirty decision, surviving
    // only by an incidental path-name match in `cell_durability`. Marking is
    // an epoch bump and is safe to repeat. It belongs inside the wrapper, not
    // at the call sites, so future callers are covered automatically.
    crate::cell_durability::mark_rdf_store_written(store);
    Ok(())
}

/// `INSERT/DELETE DATA { body }` → the same verb wrapped in
/// `GRAPH <iri> { body }`. Port of the graph MO's `graph_wrap_update`: match the
/// leading verb, take the body between the FIRST `{` and the LAST `}`, re-emit
/// wrapped. `render_updates` satisfies this shape.
fn wrap_data_body(update: &str, named_graph: &str) -> Result<String, String> {
    let trimmed = update.trim_start();
    let (verb_word, after) = if let Some(rest) = trimmed.strip_prefix("INSERT DATA") {
        ("INSERT DATA", rest)
    } else if let Some(rest) = trimmed.strip_prefix("DELETE DATA") {
        ("DELETE DATA", rest)
    } else {
        return Err(format!(
            "unexpected class update shape: {}",
            &update.chars().take(80).collect::<String>()
        ));
    };
    let open = after
        .find('{')
        .ok_or_else(|| "class update missing opening brace".to_string())?;
    let close = update
        .rfind('}')
        .ok_or_else(|| "class update missing closing brace".to_string())?;
    let body_start = update.len() - after.len() + open + 1;
    if body_start > close {
        return Err("class update has empty/invalid body span".to_string());
    }
    let body = &update[body_start..close];
    Ok(format!(
        "{verb_word} {{ GRAPH <{named_graph}> {{\n{body}\n}} }}"
    ))
}

/// The class-reconcile primitive: [`class_diff`] then, if non-empty,
/// [`apply_diff`]; returns the [`TripleDiff`] it applied. A converged class emits
/// 0 ops (the `is_empty` early-out — the SPARQL is SKIPPED, matching
/// `class_converged_zero_ops`). The op count (`removes + adds`, via
/// [`TripleDiff::op_count`]) is one face of it, projected at the consumer.
pub(crate) fn reconcile_class(
    store: &Store,
    scope: &ClassScope,
    desired: &[Triple],
) -> Result<TripleDiff, String> {
    reconcile_class_validated(store, scope, desired, None)
}

/// [`reconcile_class`] with the EA-1 SHACL VALIDATION SEAM wired in. When
/// `contract` is `Some`, the `desired` graph is validated against the SHACL
/// shapes DERIVED from that vocab contract ([`validate_desired`]) AFTER the diff
/// is computed but BEFORE any write — a LOUD HALT (`Err("SHACL: …")`, no partial
/// write) on the first conformance failure, matching the appliers' deliberate
/// fail-loud inversion. `None` is the legacy path (salience/song are not driven
/// by a single vocab contract); it is byte-for-byte the old behaviour.
///
/// The validation order is load-bearing: `desired` is the authoritative target
/// state, so it is validated as a WHOLE before `apply_diff` runs (validate the
/// target, not the delta; never validate-after-write).
pub(crate) fn reconcile_class_validated(
    store: &Store,
    scope: &ClassScope,
    desired: &[Triple],
    contract: Option<&VocabularyContract>,
) -> Result<TripleDiff, String> {
    let diff = class_diff(store, scope, desired)?;
    // ── EA-1 SHACL validation seam: after diff, BEFORE apply ──
    if let Some(contract) = contract {
        validate_desired(desired, contract)?;
    }
    if !diff.is_empty() {
        apply_diff(store, &scope.placement, &diff)?;
    }
    Ok(diff)
}

/// The MULTI-CLASS composition: reconcile a MO that is the UNION of several
/// disjoint `Fixed` rdf:type class spans (song = Song ∪ SongVerse ∪ SongCoda) in
/// ONE pass. The Rust image of the proven disjoint-class composition
/// (`L5_disjoint_class_compose`): distinct rdf:type classes own disjoint
/// subject-sets, so [`class_diff`]-ing each over ITS OWN desired subset and
/// merging the deltas is the same as reconciling each independently.
///
/// SURVEY-ALL-THEN-APPLY-ONCE: every scope's [`class_diff`] (survey + value-diff,
/// NO apply) is computed first and merged into a single [`TripleDiff`] (extend
/// removes, extend adds); then — only if the merge is non-empty — [`apply_diff`]
/// runs ONCE against the SHARED placement. Returns the merged diff (an empty diff
/// = the whole MO is converged → zero SPARQL).
///
/// PRECONDITION: every scope shares ONE placement (asserted) — song's three
/// classes all live in the DEFAULT graph (the salience WART), so the single
/// apply is unambiguous. CRITICAL: each `(scope, desired_subset)` pair MUST carry
/// ONLY the triples whose subject is typed by THAT scope's class (the caller
/// partitions `desired` by subject rdf:type) — else a sister class's triples read
/// as spurious ADDs against this class's (correctly empty) survey.
///
/// Reusable by workspace next (its classes survey-all-then-apply-once the same way).
pub(crate) fn reconcile_classes(
    store: &Store,
    scopes_desireds: &[(ClassScope, Vec<Triple>)],
) -> Result<TripleDiff, String> {
    reconcile_classes_validated(store, scopes_desireds, None)
}

/// [`reconcile_classes`] with the EA-1 SHACL VALIDATION SEAM. When `contract` is
/// `Some`, the FULL desired union (every scope's desired subset concatenated) is
/// validated ONCE — after all diffs are computed + merged, BEFORE the single
/// apply — against the contract's derived shapes. Validating the union once (not
/// per-class) is correct because the scopes own disjoint subject-sets, so the
/// union is exactly the complete desired state the single `apply_diff` writes;
/// per-class validation would also risk flagging cross-class references that are
/// only resolvable in the union. `None` is the legacy (song) path, unchanged.
pub(crate) fn reconcile_classes_validated(
    store: &Store,
    scopes_desireds: &[(ClassScope, Vec<Triple>)],
    contract: Option<&VocabularyContract>,
) -> Result<TripleDiff, String> {
    let shared_placement = match scopes_desireds.first() {
        Some((scope, _)) => scope.placement.clone(),
        // No classes ⇒ nothing to reconcile (a converged empty MO).
        None => return Ok(TripleDiff::default()),
    };

    let mut merged = TripleDiff::default();
    let mut desired_union: Vec<Triple> = Vec::new();
    for (scope, desired_subset) in scopes_desireds {
        // All scopes MUST share the one placement so the single apply below is
        // unambiguous (song = all-Default; the WART is uniform across its classes).
        assert!(
            same_placement(&shared_placement, &scope.placement),
            "reconcile_classes: every scope must share one placement (apply-once)"
        );
        let d = class_diff(store, scope, desired_subset)?;
        merged.removes.extend(d.removes);
        merged.adds.extend(d.adds);
        desired_union.extend(desired_subset.iter().cloned());
    }

    // ── EA-1 SHACL validation seam: after merge, ONCE over the full desired
    // union, BEFORE the single apply ──
    if let Some(contract) = contract {
        validate_desired(&desired_union, contract)?;
    }

    if !merged.is_empty() {
        apply_diff(store, &shared_placement, &merged)?;
    }
    Ok(merged)
}

/// Same-placement equality for the [`reconcile_classes`] shared-placement
/// assertion (`Placement` is not `PartialEq`; we only need the song-relevant
/// arms). Default == Default; Named(a) == Named(b) iff the IRIs match.
fn same_placement(a: &Placement, b: &Placement) -> bool {
    match (a, b) {
        (Placement::Default, Placement::Default) => true,
        (Placement::Named(x), Placement::Named(y)) => x == y,
        _ => false,
    }
}

// ===========================================================================
// EA-1 SHACL SEAM — END-TO-END TEACHING TEST over a REAL seeded Oxigraph store.
//
// The validator unit tests (shacl_validator.rs) prove the rudof engine accepts
// well-formed and rejects malformed `desired` sets. THESE tests prove the SEAM
// itself has teeth at the reconcile primitive: that `reconcile_class_validated`
// (a) writes a conformant desired through to the store, and (b) LOUD-HALTS on a
// malformed desired WITHOUT performing any partial write. No mocks — a real
// `Store::new()`, the real memory-core contract, the real reconcile path.
// ===========================================================================
#[cfg(test)]
mod shacl_seam_tests {
    use super::*;
    use oxigraph::model::{Literal, NamedNode};

    use crate::emporium::contract::memory_core_vocabulary;
    use crate::emporium::terms::Term;

    const MEM: &str = "http://mnemosyne.dev/memory#";
    const RDF_TYPE_URI: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    const PROJ: &str = "urn:mnemosyne:local:graph:lab:projection:memory";

    fn uri(s: &str) -> Term {
        Term::Uri(NamedNode::new(s).expect("valid IRI"))
    }
    fn string(s: &str) -> Term {
        Term::Lit(Literal::new_simple_literal(s))
    }

    /// A scope over `mem:SourceReference` subjects in the memory projection graph.
    fn source_ref_scope() -> ClassScope {
        ClassScope {
            placement: Placement::Named(PROJ.to_string()),
            key: SpanKey::Fixed {
                rdf_type: format!("{MEM}SourceReference"),
            },
            graph_id_conjunct: None,
            subjects: None,
        }
    }

    /// Count rows in the projection graph (the store-truth probe).
    fn count_proj(store: &Store) -> usize {
        let q = format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{PROJ}> {{ ?s ?p ?o }} }}");
        match SparqlEvaluator::new()
            .parse_query(&q)
            .expect("parse")
            .on_store(store)
            .execute()
            .expect("execute")
        {
            QueryResults::Solutions(s) => s.count(),
            _ => panic!("expected solutions"),
        }
    }

    fn well_formed_source_ref() -> Vec<Triple> {
        let s = format!("{PROJ}:src:abc");
        vec![
            (
                s.clone(),
                RDF_TYPE_URI.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s.clone(),
                RDF_TYPE_URI.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
            (s, format!("{MEM}sourceKind"), string("DocumentBlock")),
        ]
    }

    /// (a) A conformant desired reconciles through the seam and LANDS in the store.
    #[test]
    fn conformant_desired_validates_and_writes() {
        let store = Store::new().expect("in-memory store");
        let contract = memory_core_vocabulary();
        let desired = well_formed_source_ref();

        let diff = reconcile_class_validated(&store, &source_ref_scope(), &desired, Some(contract))
            .expect("conformant desired must pass SHACL and write");
        assert!(diff.op_count() > 0, "fresh reconcile writes the footprint");
        assert_eq!(
            count_proj(&store),
            3,
            "all three triples landed in the projection graph"
        );
    }

    /// (b) TEETH: a malformed desired (missing the required mem:sourceKind) is
    /// LOUD-REJECTED and writes NOTHING — the store stays empty (no partial write).
    #[test]
    fn malformed_desired_loud_halts_with_no_write() {
        let store = Store::new().expect("in-memory store");
        let contract = memory_core_vocabulary();
        let s = format!("{PROJ}:src:bad");
        let malformed = vec![
            (
                s.clone(),
                RDF_TYPE_URI.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s,
                RDF_TYPE_URI.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
            // mem:sourceKind (required) deliberately OMITTED.
        ];

        let result =
            reconcile_class_validated(&store, &source_ref_scope(), &malformed, Some(contract));
        assert!(
            result.is_err(),
            "malformed desired must be rejected by the seam"
        );
        let msg = result.unwrap_err();
        assert!(msg.starts_with("SHACL:"), "loud-halt prefix: {msg}");
        assert_eq!(
            count_proj(&store),
            0,
            "NO partial write on a halted reconcile"
        );
    }

    /// CONTROL: the SAME malformed desired passes the legacy (contract=None) path —
    /// proving the rejection is the SEAM's doing, not some unrelated reconcile error.
    #[test]
    fn malformed_desired_passes_when_validation_is_off() {
        let store = Store::new().expect("in-memory store");
        let s = format!("{PROJ}:src:bad");
        let malformed = vec![
            (
                s.clone(),
                RDF_TYPE_URI.to_string(),
                uri(&format!("{MEM}SourceReference")),
            ),
            (
                s,
                RDF_TYPE_URI.to_string(),
                uri("http://www.w3.org/ns/prov#Entity"),
            ),
        ];
        let diff = reconcile_class_validated(&store, &source_ref_scope(), &malformed, None)
            .expect("legacy path applies without validation");
        assert!(diff.op_count() > 0);
        assert_eq!(
            count_proj(&store),
            2,
            "legacy path wrote the (malformed) triples"
        );
    }
}
