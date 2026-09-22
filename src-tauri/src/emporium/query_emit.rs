//! Vocab → query-plan emitter — the QUERY FACE of a Meaningful-Objects vocab
//! contract (T2 item 11). A sibling of [`super::shacl_emit`] /
//! [`super::openapi_emit`]: a PURE function of a parsed [`VocabularyContract`]
//! + one of its classes, deriving the four canonical named query PLANS —
//! never hand-written per class, never re-derived per consumer.
//!
//! ```text
//! byId(subject)        — the shaped record for one subject
//! currentHeads({asOf}) — current heads; asOf ⇒ historical heads
//! lineageOf(subject)   — the supersession/contradiction chain
//! countBy(attr)        — attr must be one of the class's declared sh:path properties
//! ```
//!
//! This module only EMITS the catalog (names, params, human-readable SPARQL
//! templates) — it never touches a store. Execution lives in
//! [`super::query_engine`], which renders these SAME templates with concrete
//! graph IRIs + parameter values and runs them (the shacl_emit/shacl_validator
//! split, mirrored here).
//!
//! **Scoping is signature-derived, never a mode-string fork**: every plan's
//! graph pattern comes from [`super::class_dispatch::resolve`] reading the
//! class's declared `store_mode`/`store_target` — never the class's name, the
//! vocab's name, or a hand-maintained table. A `VirtualSkip` class (nothing
//! materialized) gets a catalog where every plan is marked `unsupported` with
//! the reason, never a plan that would silently run and return nothing.
//!
//! **`currentHeads`/`lineageOf` are signature-derived too**: they require the
//! class's OWN shape to declare the lineage convention (`lineage`/`isCurrent`,
//! plus `createdAt`/`supersedes` for `asOf`/the chain walk) — checked by LOCAL
//! predicate name, not hardcoded to the `mem:` prefix, so any future pack that
//! adopts the same convention gets these plans for free. A class without the
//! convention (e.g. `Bookmark`) still gets a catalog entry for these query
//! names, but `unsupported` names exactly which predicate is missing — the
//! "kill the silent zero" doctrine applied to DISCOVERY, not just execution.

use std::collections::BTreeSet;

use crate::emporium::class_dispatch::{self, DispatchRoute};
use crate::emporium::contract::{ClassSpec, VocabularyContract};

/// Query-face errors, carrying the HTTP-ish status the route/MCP layers map —
/// mirrors [`crate::emporium::objects::ObjectError`] exactly (the same
/// "structured rejection, never a silent empty" shape).
#[derive(Debug)]
pub(crate) enum QueryEmitError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl QueryEmitError {
    pub(crate) fn status(&self) -> u16 {
        match self {
            QueryEmitError::BadRequest(_) => 400,
            QueryEmitError::NotFound(_) => 404,
            QueryEmitError::Internal(_) => 500,
        }
    }
    pub(crate) fn message(&self) -> &str {
        match self {
            QueryEmitError::BadRequest(m)
            | QueryEmitError::NotFound(m)
            | QueryEmitError::Internal(m) => m,
        }
    }
}

/// One parameter a named query accepts.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueryParam {
    pub(crate) name: &'static str,
    pub(crate) required: bool,
    pub(crate) description: String,
}

/// One canonical named query plan for a class.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueryPlan {
    pub(crate) name: &'static str,
    pub(crate) description: String,
    pub(crate) params: Vec<QueryParam>,
    /// A human-readable SPARQL template: structural parts (graphs, predicate
    /// URIs, rdf:type) are the REAL expanded values (fixed by the contract);
    /// call-time parameters are shown as `{token}` placeholders (matching the
    /// house `subject_rule` token convention). [`super::query_engine`] renders
    /// the SAME shape with concrete values substituted for the tokens.
    pub(crate) sparql_template: String,
    /// `None` when this plan runs for this class; `Some(reason)` when the
    /// class's declared shape/signature does not support it — the catalog
    /// SAYS so rather than omitting the plan (discovery must never be a
    /// silent gap either).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) unsupported: Option<String>,
}

/// The full catalog for one class.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueryCatalog {
    pub(crate) vocab: String,
    pub(crate) class: String,
    /// True for a membrane-ed class (today: `store_target == "projection:memory"`):
    /// a supplied `observer` widens every plan's scope to commons ∪ that
    /// observer's membrane graph (RATIFIED default: no observer ⇒ commons only).
    pub(crate) membrane_aware: bool,
    pub(crate) queries: Vec<QueryPlan>,
}

/// The class's own declared predicates that carry the §8.2 lineage
/// convention — resolved by LOCAL NAME (not hardcoded to `mem:`), so any pack
/// that adopts the same shape gets `currentHeads`/`lineageOf` for free.
/// `pub(crate)` so [`super::query_engine`] shares this EXACT derivation
/// instead of re-deriving which predicates carry lineage semantics.
pub(crate) struct LineageShape {
    pub(crate) lineage: String,
    pub(crate) is_current: String,
    pub(crate) created_at: Option<String>,
    pub(crate) supersedes: Option<String>,
}

/// Find a class's own declared predicate whose CURIE local name (the part
/// after the ':') matches `local` exactly, expanded to its full URI.
fn predicate_uri_by_local_name(
    contract: &VocabularyContract,
    spec: &ClassSpec,
    local: &str,
) -> Option<String> {
    spec.predicates
        .keys()
        .find(|curie| curie.rsplit(':').next() == Some(local))
        .and_then(|curie| contract.expand(curie).ok())
}

pub(crate) fn lineage_shape(
    contract: &VocabularyContract,
    spec: &ClassSpec,
) -> Option<LineageShape> {
    let lineage = predicate_uri_by_local_name(contract, spec, "lineage")?;
    let is_current = predicate_uri_by_local_name(contract, spec, "isCurrent")?;
    Some(LineageShape {
        lineage,
        is_current,
        created_at: predicate_uri_by_local_name(contract, spec, "createdAt"),
        supersedes: predicate_uri_by_local_name(contract, spec, "supersedes"),
    })
}

/// Every `attr` name legal for `countBy` on this class: each declared
/// predicate's CURIE, its bare local name, and its expanded URI all resolve
/// (the engine accepts any of the three forms).
pub(crate) fn class_attr_curies(spec: &ClassSpec) -> Vec<String> {
    spec.predicates.keys().cloned().collect()
}

/// The signature-derived scope-graph TEMPLATE (a display string, using the
/// `{graph_subject}` token) for a class's resolved [`DispatchRoute`]. Mirrors
/// [`super::query_engine::resolve_class_scope`] exactly — this is the
/// human-readable twin of that real resolution, never a second derivation.
fn scope_graph_templates(store_target: &str, membrane_aware: bool) -> Vec<String> {
    let base = format!("{{graph_subject}}:{store_target}");
    if membrane_aware {
        vec![base.clone(), format!("{base}:agent:{{observer}}")]
    } else {
        vec![base]
    }
}

/// Splice `body` into a `GRAPH` pattern per graph, joined by `UNION` — the ONE
/// shape both this module's DISPLAY templates (`graphs` are `{token}` text)
/// and [`super::query_engine`]'s EXECUTION queries (`graphs` are real IRIs)
/// render through, so the catalog a caller reads is never a second, drifting
/// derivation of what actually runs. `pub(crate)` for that reuse.
pub(crate) fn union_over_graphs(graphs: &[String], body: &str) -> String {
    graphs
        .iter()
        .map(|g| format!("{{ GRAPH <{g}> {{ {body} }} }}"))
        .collect::<Vec<_>>()
        .join("\n  UNION\n  ")
}

/// Derive the full [`QueryCatalog`] for one class — pure, deterministic,
/// no I/O. Errors exactly where [`class_dispatch::resolve`] would (an
/// unknown/malformed class).
pub(crate) fn emit_query_catalog(
    contract: &VocabularyContract,
    class_name: &str,
) -> Result<QueryCatalog, QueryEmitError> {
    let spec = contract
        .classes
        .get(class_name)
        .ok_or_else(|| {
            QueryEmitError::NotFound(format!(
                "class '{class_name}' is not declared by vocab '{}'",
                contract.name
            ))
        })?
        .clone();
    let dispatch =
        class_dispatch::resolve(contract, class_name).map_err(QueryEmitError::BadRequest)?;

    let virtual_reason = matches!(dispatch.route, DispatchRoute::VirtualSkip).then(|| {
        format!(
            "class '{class_name}' is virtual (store_mode=virtual, derived_from_query={:?}) — \
             nothing is materialized to query; these plans do not apply",
            spec.derived_from_query.as_deref().unwrap_or("")
        )
    });
    let membrane_aware = dispatch.signature.store_target == "projection:memory";
    let graphs = scope_graph_templates(dispatch.signature.store_target, membrane_aware);
    let lineage = lineage_shape(contract, &spec);

    let mut queries = Vec::new();

    // ── byId(subject) ──
    queries.push(QueryPlan {
        name: "byId",
        description: "The shaped record for one subject: every declared (predicate, value) \
                       pair on it, scoped to this class's resolved sink."
            .to_string(),
        params: vec![QueryParam {
            name: "subject",
            required: true,
            description: "The full subject IRI to read.".to_string(),
        }],
        sparql_template: format!(
            "SELECT ?p ?o WHERE {{\n  {}\n}}",
            union_over_graphs(&graphs, "<{subject}> ?p ?o")
        ),
        unsupported: virtual_reason.clone(),
    });

    // ── countBy(attr) ──
    let legal_attrs = class_attr_curies(&spec);
    let count_by_unsupported = virtual_reason.clone().or_else(|| {
        legal_attrs
            .is_empty()
            .then(|| format!("class '{class_name}' declares no predicates to count by"))
    });
    queries.push(QueryPlan {
        name: "countBy",
        description: format!(
            "Group-count instances by one declared property. Legal `attr` values (this \
             class's own sh:path properties): {}.",
            if legal_attrs.is_empty() {
                "(none)".to_string()
            } else {
                legal_attrs.join(", ")
            }
        ),
        params: vec![QueryParam {
            name: "attr",
            required: true,
            description: "One of this class's declared predicate CURIEs (or bare local name, \
                           or full URI) — validated against the shape; never a free string."
                .to_string(),
        }],
        sparql_template: format!(
            "SELECT ?value (COUNT(?s) AS ?count) WHERE {{\n  {}\n}}\nGROUP BY ?value ORDER BY DESC(?count)",
            union_over_graphs(&graphs, "?s a <{rdfType}> ; <{attr}> ?value")
        ),
        unsupported: count_by_unsupported,
    });

    // ── currentHeads({asOf?}) ──
    let heads_unsupported = virtual_reason.clone().or_else(|| {
        lineage.is_none().then(|| {
            format!(
                "class '{class_name}' declares no `lineage`/`isCurrent` predicate pair — \
                 there is no supersession lineage to report heads over"
            )
        })
    });
    let heads_template = match &lineage {
        Some(l) => format!(
            "# live (no asOf): the ratified \"Head = isCurrent=true\" reader\nSELECT ?lineage ?s WHERE {{\n  {}\n}}\n\n\
             # historical (asOf given): reconstructed from createdAt + supersedes\nSELECT ?lineage ?s WHERE {{\n  {}\n}}",
            union_over_graphs(&graphs, &format!("?s <{}> ?lineage ; <{}> true", l.lineage, l.is_current)),
            union_over_graphs(
                &graphs,
                &format!(
                    "?s <{}> ?lineage ; <{}> ?createdAt . FILTER(?createdAt <= {{asOf}}) \
                     FILTER NOT EXISTS {{ ?s2 <{}> ?s ; <{}> ?createdAt2 . FILTER(?createdAt2 <= {{asOf}}) }}",
                    l.lineage,
                    l.created_at.as_deref().unwrap_or("{createdAtPredicateUndeclared}"),
                    l.supersedes.as_deref().unwrap_or("{supersedesPredicateUndeclared}"),
                    l.created_at.as_deref().unwrap_or("{createdAtPredicateUndeclared}"),
                )
            )
        ),
        None => String::new(),
    };
    let heads_asof_gap = lineage.as_ref().and_then(|l| {
        (l.created_at.is_none() || l.supersedes.is_none())
            .then(|| " `asOf` is unavailable for this class (no createdAt/supersedes predicate); only the live read runs.".to_string())
    });
    queries.push(QueryPlan {
        name: "currentHeads",
        description: format!(
            "Every current head, grouped by lineage, flagged `contested` when a lineage has \
             more than one (the §8.1 \"return all heads flagged\" policy — storage never picks \
             a winner).{}",
            heads_asof_gap.unwrap_or_default()
        ),
        params: vec![QueryParam {
            name: "asOf",
            required: false,
            description: "Epoch-millis or ISO-8601 datetime. Omitted ⇒ the live isCurrent-based \
                           read; given ⇒ the historical reconstruction as of that instant."
                .to_string(),
        }],
        sparql_template: heads_template,
        unsupported: heads_unsupported,
    });

    // ── lineageOf(subject) ──
    let lineage_of_unsupported = virtual_reason.clone().or_else(|| match &lineage {
        None => Some(format!(
            "class '{class_name}' declares no `lineage`/`isCurrent` predicate pair"
        )),
        Some(l) if l.supersedes.is_none() => Some(format!(
            "class '{class_name}' declares no `supersedes` predicate — there is no chain to walk"
        )),
        Some(_) => None,
    });
    let lineage_of_template = match lineage.as_ref().filter(|l| l.supersedes.is_some()) {
        Some(l) => {
            let supersedes = l.supersedes.as_deref().unwrap_or_default();
            format!(
                "SELECT DISTINCT ?node WHERE {{\n  {}\n}}",
                union_over_graphs(
                    &graphs,
                    &format!("<{{subject}}> (<{supersedes}>|^<{supersedes}>)* ?node")
                )
            )
        }
        None => String::new(),
    };
    queries.push(QueryPlan {
        name: "lineageOf",
        description: "The full supersession chain reachable from one subject (every version \
                       ever produced, forward and backward) — the §8.2 lineage, walked live."
            .to_string(),
        params: vec![QueryParam {
            name: "subject",
            required: true,
            description: "The full subject IRI to walk the chain from.".to_string(),
        }],
        sparql_template: lineage_of_template,
        unsupported: lineage_of_unsupported,
    });

    Ok(QueryCatalog {
        vocab: contract.name.clone(),
        class: class_name.to_string(),
        membrane_aware,
        queries,
    })
}

/// Validate a caller-supplied `attr` (CURIE, bare local name, or full URI)
/// against a class's declared shape, returning the EXPANDED predicate URI.
/// Loud rejection (never a silent empty result from an unrecognized `attr`).
pub(crate) fn resolve_count_by_attr(
    contract: &VocabularyContract,
    spec: &ClassSpec,
    attr: &str,
) -> Result<String, QueryEmitError> {
    let attr = attr.trim();
    if attr.is_empty() {
        return Err(QueryEmitError::BadRequest(
            "countBy requires a non-empty 'attr' param".to_string(),
        ));
    }
    // 1) an exact declared CURIE.
    if spec.predicates.contains_key(attr) {
        if let Ok(uri) = contract.expand(attr) {
            return Ok(uri);
        }
    }
    // 2) a full expanded URI already matching one of the class's own predicates.
    let known: BTreeSet<String> = spec
        .predicates
        .keys()
        .filter_map(|curie| contract.expand(curie).ok())
        .collect();
    if known.contains(attr) {
        return Ok(attr.to_string());
    }
    // 3) a bare local name.
    if let Some(curie) = spec
        .predicates
        .keys()
        .find(|curie| curie.rsplit(':').next() == Some(attr))
    {
        if let Ok(uri) = contract.expand(curie) {
            return Ok(uri);
        }
    }
    Err(QueryEmitError::BadRequest(format!(
        "'{attr}' is not a declared property of class '{}' — legal values: {}",
        spec.rdf_types.first().cloned().unwrap_or_default(),
        class_attr_curies(spec).join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::contract::{get_vocabulary, memory_core_vocabulary};

    #[test]
    fn memory_record_gets_all_four_supported_plans() {
        let contract = memory_core_vocabulary();
        let catalog = emit_query_catalog(contract, "MemoryRecord").expect("MemoryRecord resolves");
        assert_eq!(catalog.vocab, "sophia-memory-core");
        assert_eq!(catalog.class, "MemoryRecord");
        assert!(catalog.membrane_aware, "memory sink is membrane-aware");
        assert_eq!(catalog.queries.len(), 4);
        for plan in &catalog.queries {
            assert!(
                plan.unsupported.is_none(),
                "{} should be supported on MemoryRecord: {:?}",
                plan.name,
                plan.unsupported
            );
            assert!(
                !plan.sparql_template.is_empty(),
                "{} has a template",
                plan.name
            );
        }
        let names: Vec<&str> = catalog.queries.iter().map(|q| q.name).collect();
        assert_eq!(names, vec!["byId", "countBy", "currentHeads", "lineageOf"]);
    }

    #[test]
    fn memory_record_scope_is_membrane_templated() {
        let contract = memory_core_vocabulary();
        let catalog = emit_query_catalog(contract, "MemoryRecord").expect("resolves");
        let by_id = catalog.queries.iter().find(|q| q.name == "byId").unwrap();
        assert!(by_id
            .sparql_template
            .contains("{graph_subject}:projection:memory"));
        assert!(by_id
            .sparql_template
            .contains("{graph_subject}:projection:memory:agent:{observer}"));
    }

    #[test]
    fn simple_class_without_lineage_marks_heads_and_chain_unsupported() {
        let contract = get_vocabulary("emporium-bookmark").expect("emporium-bookmark registered");
        let catalog = emit_query_catalog(contract, "Bookmark").expect("Bookmark resolves");
        assert!(
            !catalog.membrane_aware,
            "bookmark sink has no observer membrane"
        );
        let by_name = |name: &str| catalog.queries.iter().find(|q| q.name == name).unwrap();
        assert!(by_name("byId").unsupported.is_none());
        assert!(by_name("countBy").unsupported.is_none());
        assert!(
            by_name("currentHeads").unsupported.is_some(),
            "Bookmark has no lineage/isCurrent predicate"
        );
        assert!(
            by_name("lineageOf").unsupported.is_some(),
            "Bookmark has no supersedes predicate"
        );
    }

    // ── N1 landing-brief step 5: the lineage convention generalizes to `lex:` ──

    /// `DoctrineHead` declares local names `lineage`/`isCurrent`/`createdAt`/
    /// `supersedes` BY DESIGN (the landing brief's "known constraint": these
    /// exact local names are load-bearing, `query_emit.rs:136-148` keys on them).
    /// `lineage_shape` must recognize it by LOCAL NAME resolution alone (never a
    /// hardcoded `mem:` check), so `currentHeads`/`lineageOf` are SUPPORTED.
    #[test]
    fn doctrine_head_lineage_shape_is_recognized_by_local_name() {
        let contract = get_vocabulary("lex-scotus-core").expect("lex-scotus-core registered");
        let catalog = emit_query_catalog(contract, "DoctrineHead").expect("DoctrineHead resolves");
        let by_name = |name: &str| catalog.queries.iter().find(|q| q.name == name).unwrap();
        assert!(
            by_name("currentHeads").unsupported.is_none(),
            "DoctrineHead carries lex:lineage/lex:isCurrent — currentHeads must be SUPPORTED"
        );
        assert!(
            by_name("lineageOf").unsupported.is_none(),
            "DoctrineHead carries lex:supersedes — lineageOf must be SUPPORTED"
        );
        // asOf is available too (createdAt + supersedes both declared).
        assert!(!by_name("currentHeads")
            .description
            .contains("`asOf` is unavailable"));
    }

    /// `Case` declares NO lineage/isCurrent predicate pair at all (treatment
    /// edges like `lex:overrules` are plain Case-to-Case edges, not head
    /// machinery — the landing brief's other "known constraint"). It must mark
    /// currentHeads/lineageOf unsupported-WITH-REASON, exactly like Bookmark.
    #[test]
    fn case_has_no_lineage_shape_and_marks_heads_unsupported_with_reason() {
        let contract = get_vocabulary("lex-scotus-core").expect("lex-scotus-core registered");
        let catalog = emit_query_catalog(contract, "Case").expect("Case resolves");
        let by_name = |name: &str| catalog.queries.iter().find(|q| q.name == name).unwrap();
        let heads = by_name("currentHeads");
        assert!(heads.unsupported.is_some(), "Case has no lineage shape");
        assert!(
            heads
                .unsupported
                .as_ref()
                .unwrap()
                .contains("no `lineage`/`isCurrent` predicate pair"),
            "{:?}",
            heads.unsupported
        );
        let lineage_of = by_name("lineageOf");
        assert!(
            lineage_of.unsupported.is_some(),
            "Case has no lineage shape"
        );
    }

    #[test]
    fn virtual_class_marks_every_plan_unsupported() {
        let contract = get_vocabulary("workflow").expect("workflow registered");
        // `Draft` is virtual (store_mode=virtual) per class_dispatch's snapshot table.
        let catalog = emit_query_catalog(contract, "Draft").expect("Draft resolves (virtual)");
        for plan in &catalog.queries {
            assert!(
                plan.unsupported.is_some(),
                "{} must be marked unsupported on a virtual class",
                plan.name
            );
            assert!(plan.unsupported.as_ref().unwrap().contains("virtual"));
        }
    }

    #[test]
    fn unknown_class_is_a_structured_not_found() {
        let contract = memory_core_vocabulary();
        let err = emit_query_catalog(contract, "NoSuchClass").unwrap_err();
        assert_eq!(err.status(), 404);
        assert!(err.message().contains("NoSuchClass"));
    }

    #[test]
    fn count_by_attr_resolves_curie_local_name_and_uri_forms() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        let via_curie = resolve_count_by_attr(contract, spec, "mem:status").unwrap();
        let via_local = resolve_count_by_attr(contract, spec, "status").unwrap();
        let via_uri = resolve_count_by_attr(contract, spec, &via_curie).unwrap();
        assert_eq!(via_curie, "http://mnemosyne.dev/memory#status");
        assert_eq!(via_curie, via_local);
        assert_eq!(via_curie, via_uri);
    }

    #[test]
    fn count_by_attr_rejects_undeclared_property_loudly() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        let err = resolve_count_by_attr(contract, spec, "notAField").unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.message().contains("notAField"));
        assert!(err.message().contains("legal values"));
    }

    #[test]
    fn catalog_emission_is_deterministic() {
        let contract = memory_core_vocabulary();
        let a = emit_query_catalog(contract, "MemoryRecord").unwrap();
        let b = emit_query_catalog(contract, "MemoryRecord").unwrap();
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap()
        );
    }
}
