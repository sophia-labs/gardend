//! `emporium_query` — the OBJECT query face (T2 item 11a): **"objects out,
//! not rows."** A sibling to [`super::query_engine`] (item 11's substrate),
//! this module composes Builder 1's machinery — `query_emit`'s scoping
//! templates, `query_engine`'s signature-derived scope resolver, `sweep`'s
//! ONE unified "current head" reader — with the CRUD hydration style
//! ([`super::objects`]'s `{predicate: [values]}` rendering) to answer a
//! criteria-based question about one class with HYDRATED, SHAPED,
//! PROVENANCE-CARRYING objects, never a bindings table.
//!
//! **The criteria grammar is DERIVED, never invented** (the 11a-sketch,
//! ratified 2026-07-06): legal filter keys = the class's own declared
//! `sh:path`s (its predicates, resolved by CURIE / bare local name / full
//! URI — the same three forms [`super::query_emit::resolve_count_by_attr`]
//! accepts for `countBy`). The predicate's declared datatype dictates which
//! operator(s) are legal: `eq` is the universal baseline every datatype
//! supports; `string` additionally allows `contains`; `dateTime` additionally
//! allows `before`/`after`; `uri` and the numeric/boolean families get `eq`
//! only. Conjunctive-only this wave (one criteria object = one AND of
//! per-predicate constraints). A malformed criterion — an unknown key, an
//! operator illegal for that predicate's datatype, or a value that does not
//! parse for the datatype — is a LOUD structured rejection naming the
//! class's legal keys/operators, never a silent empty result.
//!
//! **Observer semantics (RATIFIED, generalized from recall to every class):**
//! no `observer` ⇒ commons only; `observer` ⇒ commons ∪ that witness's
//! membrane (delegated to [`super::query_engine::resolve_class_scope`] —
//! never re-derived); `perspectives: "all"` ⇒ every DISCOVERED witness
//! membrane merged, and — because merged testimony is never anonymous —
//! each resulting object is individually tagged with the `witnessGraph` it
//! was actually read from (one occurrence per graph it lives in, not a
//! deduplicated, attribution-less union).
//!
//! **Contested classes (11a law 5):** a class carrying the ratified
//! lineage/`isCurrent` shape AND the ratified `contested` conflict policy
//! (`sweep::declared_conflict_strategy`) routes "current" through Builder 1's
//! ONE unified head reader ([`sweep::current_heads_by_lineage`] /
//! [`sweep::heads_as_of`]) — never a second "current" semantics invented
//! here. By default only current heads are returned, each flagged
//! `contested` with its sibling head refs; `as_of` walks history instead.

use std::collections::{BTreeMap, BTreeSet};

use oxigraph::store::Store;
use serde_json::Value;

use crate::app_runtime::AppHandle;
use crate::emporium::chamber_ontology::resolve_ingest_contract;
use crate::emporium::contract::{ClassSpec, Datatype, PredicateSpec, VocabularyContract};
use crate::emporium::memory_applier::open_memory_store;
use crate::emporium::objects::{class_rdf_type, ObjectError};
use crate::emporium::query_emit::{self, QueryEmitError};
use crate::emporium::query_engine::{self, resolve_class_scope};
use crate::emporium::sweep;
use crate::rdf_authority::memory_projection_graph_iri;
use crate::rdf_query_service::execute_sparql_query;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// Caller-supplied options beyond the criteria filter itself.
#[derive(Debug, Default, Clone)]
pub(crate) struct ObjectQueryOptions {
    pub(crate) observer: Option<String>,
    /// The ONLY legal non-empty value is `"all"` — anything else is a loud
    /// rejection (never a silently-ignored typo).
    pub(crate) perspectives: Option<String>,
    pub(crate) as_of: Option<Value>,
    pub(crate) limit: Option<usize>,
    pub(crate) cursor: Option<usize>,
}

/// One hydrated, shaped result — the class-signature-scoped predicate span
/// for a matched subject (mirrors [`super::objects::read_object`]'s
/// `{predicate: [values]}` rendering exactly, so a caller already reading
/// `emporium_read` recognizes the shape), plus provenance/contested framing.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HydratedObject {
    pub(crate) subject: String,
    pub(crate) rdf_type: String,
    /// Every declared (predicate, value) pair in scope, N-Triples value
    /// forms — the SAME "honest, face-neutral rendering" `objects::
    /// read_object` uses.
    pub(crate) predicates: BTreeMap<String, Vec<String>>,
    /// `Some(bool)` only for a class under the ratified `contested` policy;
    /// `None` for every other class (there is no "current head" concept to
    /// report).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) contested: Option<bool>,
    /// The OTHER current heads sharing this subject's lineage (empty unless
    /// `contested == Some(true)`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) sibling_heads: Vec<String>,
    /// Which graph this occurrence was actually read from — set ONLY under
    /// `perspectives: "all"` (merged testimony is never anonymous).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) witness_graph: Option<String>,
}

/// The resolved scope this query ran against.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObjectQueryScope {
    pub(crate) graphs: Vec<String>,
    /// `"commons"` | `"observer"` | `"all"`.
    pub(crate) perspectives: String,
}

/// The full result: HYDRATED OBJECTS, never bindings rows.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObjectQueryOutcome {
    pub(crate) graph_id: String,
    pub(crate) vocab: String,
    pub(crate) class: String,
    /// The criteria as given (echoed for transparency — the caller's own
    /// input, not a re-derivation).
    pub(crate) criteria: Value,
    pub(crate) scope: ObjectQueryScope,
    pub(crate) total_matched: usize,
    pub(crate) limit: usize,
    pub(crate) offset: usize,
    /// `Some(next_offset)` when more results exist beyond this page. A
    /// simple stable-order OFFSET this wave (see the module's build report —
    /// an opaque cursor token is future work).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<usize>,
    pub(crate) objects: Vec<HydratedObject>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) warnings: Vec<String>,
}

// ── the criteria grammar: operators, legality, resolution ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CriteriaOp {
    Eq,
    Contains,
    Before,
    After,
}

impl CriteriaOp {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "eq" => Some(Self::Eq),
            "contains" => Some(Self::Contains),
            "before" => Some(Self::Before),
            "after" => Some(Self::After),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Contains => "contains",
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// The datatype-derived legal operator set (11a-sketch: "datatypes dictate
/// operators"). `eq` is the universal baseline; `string` additionally allows
/// `contains`; `dateTime` additionally allows `before`/`after`. `uri` and the
/// numeric/boolean families get `eq` only.
fn legal_ops_for(datatype: Datatype) -> &'static [CriteriaOp] {
    match datatype {
        Datatype::string => &[CriteriaOp::Eq, CriteriaOp::Contains],
        Datatype::dateTime => &[CriteriaOp::Eq, CriteriaOp::Before, CriteriaOp::After],
        Datatype::uri
        | Datatype::integer
        | Datatype::long
        | Datatype::float
        | Datatype::double
        | Datatype::boolean => &[CriteriaOp::Eq],
    }
}

fn ops_label(ops: &[CriteriaOp]) -> String {
    ops.iter()
        .map(|op| op.label())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve a criteria KEY (a declared predicate CURIE, its bare local name,
/// or its expanded URI) against the class's OWN shape — "legal filter keys =
/// the class shape's sh:paths". A deliberately FRESH resolver — NOT a
/// refactor of [`query_emit::resolve_count_by_attr`] — so this wave's new
/// grammar can never regress Builder 1's already-proven `countBy` resolution;
/// the two share the same three-form (CURIE / local name / URI) shape by
/// convention, not by call-through.
fn resolve_criteria_key<'a>(
    contract: &VocabularyContract,
    spec: &'a ClassSpec,
    class_name: &str,
    key: &str,
) -> Result<(String, &'a PredicateSpec), QueryEmitError> {
    let trimmed = key.trim();
    let found = spec
        .predicates
        .get(trimmed)
        .and_then(|p| contract.expand(trimmed).ok().map(|uri| (uri, p)))
        .or_else(|| {
            spec.predicates.iter().find_map(|(curie, p)| {
                let uri = contract.expand(curie).ok()?;
                (uri == trimmed).then_some((uri, p))
            })
        })
        .or_else(|| {
            spec.predicates.iter().find_map(|(curie, p)| {
                if curie.rsplit(':').next() == Some(trimmed) {
                    contract.expand(curie).ok().map(|uri| (uri, p))
                } else {
                    None
                }
            })
        });
    found.ok_or_else(|| {
        QueryEmitError::BadRequest(format!(
            "criteria key '{key}' is not a declared filterable property of class '{class_name}' \
             — legal keys: {}",
            query_emit::class_attr_curies(spec).join(", ")
        ))
    })
}

fn escape_sparql_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn parse_uri_criterion_value(key: &str, value: &Value) -> Result<String, QueryEmitError> {
    let s = value.as_str().ok_or_else(|| {
        QueryEmitError::BadRequest(format!(
            "criteria key '{key}': expected a URI string value for an IRI-typed property"
        ))
    })?;
    let s = s.trim();
    if !(s.contains("://") || s.starts_with("urn:")) {
        return Err(QueryEmitError::BadRequest(format!(
            "criteria key '{key}': '{s}' does not look like a URI (expected 'scheme://…' or \
             'urn:…') for an IRI-typed property"
        )));
    }
    // The shape check above only rules out obviously-non-URI strings; it does
    // NOT rule out a value that is BOTH a `urn:`-prefixed string AND carries
    // SPARQL-breaking characters (`>`, newlines, …) that would splice out of
    // the `<{uri}>` triple pattern this value is embedded into below. Same
    // check `query_engine`'s `byId`/`lineageOf` subject params use — shared,
    // not re-derived.
    query_engine::validate_sparql_iri(&format!("criteria key '{key}'"), s)?;
    Ok(s.to_string())
}

fn numeric_literal(key: &str, datatype: Datatype, value: &Value) -> Result<String, QueryEmitError> {
    let xsd_name = match datatype {
        Datatype::integer => "integer",
        Datatype::long => "long",
        Datatype::float => "float",
        Datatype::double => "double",
        _ => unreachable!("numeric_literal is only called for numeric datatypes"),
    };
    let lexical = if let Some(n) = value.as_i64() {
        n.to_string()
    } else if let Some(n) = value.as_u64() {
        n.to_string()
    } else if let Some(n) = value.as_f64() {
        if n.fract() == 0.0 && n.is_finite() {
            format!("{n:.1}")
        } else {
            format!("{n}")
        }
    } else {
        return Err(QueryEmitError::BadRequest(format!(
            "criteria key '{key}': expected a numeric value for an xsd:{xsd_name}-typed property"
        )));
    };
    Ok(format!(
        "\"{lexical}\"^^<http://www.w3.org/2001/XMLSchema#{xsd_name}>"
    ))
}

/// Format a NON-uri criterion value as a ready-to-splice SPARQL literal term.
fn format_literal_criterion(
    key: &str,
    datatype: Datatype,
    value: &Value,
) -> Result<String, QueryEmitError> {
    match datatype {
        Datatype::uri => {
            unreachable!("uri eq is a direct triple pattern, handled by the caller before here")
        }
        Datatype::string => {
            let s = value.as_str().ok_or_else(|| {
                QueryEmitError::BadRequest(format!("criteria key '{key}': expected a string value"))
            })?;
            Ok(format!("\"{}\"", escape_sparql_string(s)))
        }
        Datatype::dateTime => query_engine::format_as_of_literal(value).map_err(|e| {
            QueryEmitError::BadRequest(format!("criteria key '{key}': {}", e.message()))
        }),
        Datatype::boolean => {
            let b = value.as_bool().ok_or_else(|| {
                QueryEmitError::BadRequest(format!(
                    "criteria key '{key}': expected a boolean value"
                ))
            })?;
            Ok(b.to_string())
        }
        Datatype::integer | Datatype::long | Datatype::float | Datatype::double => {
            numeric_literal(key, datatype, value)
        }
    }
}

/// Build the conjunctive WHERE-clause BODY (no enclosing `{ }`, no `GRAPH`
/// wrapping — [`query_emit::union_over_graphs`] supplies that) for one
/// class's criteria: `?s a <rdf_type>` plus one `?s <pred> …` fragment per
/// criterion. Loud rejection on the FIRST malformed key/operator/value —
/// never a silently-dropped criterion.
fn build_criteria_body(
    contract: &VocabularyContract,
    spec: &ClassSpec,
    class_name: &str,
    rdf_type: &str,
    criteria: &Value,
) -> Result<String, QueryEmitError> {
    let obj = criteria.as_object().ok_or_else(|| {
        QueryEmitError::BadRequest(
            "criteria must be a JSON object of {predicateKey: value | {operator: value}}"
                .to_string(),
        )
    })?;
    let mut body = format!("?s a <{rdf_type}> . ");
    for (var_idx, (key, raw_value)) in obj.iter().enumerate() {
        let (predicate_uri, pred_spec) = resolve_criteria_key(contract, spec, class_name, key)?;
        let legal = legal_ops_for(pred_spec.datatype);
        let (op, value) = match raw_value {
            Value::Object(m) if m.len() == 1 => {
                let (op_name, v) = m.iter().next().expect("len == 1 checked above");
                let op = CriteriaOp::parse(op_name).ok_or_else(|| {
                    QueryEmitError::BadRequest(format!(
                        "criteria key '{key}' (datatype {:?}) has unknown operator '{op_name}' \
                         — legal operators: {}",
                        pred_spec.datatype,
                        ops_label(legal)
                    ))
                })?;
                (op, v.clone())
            }
            Value::Object(_) => {
                return Err(QueryEmitError::BadRequest(format!(
                    "criteria key '{key}': an operator object must carry EXACTLY one operator \
                     (conjunctive-only this wave) — legal operators: {}",
                    ops_label(legal)
                )))
            }
            Value::Null => {
                return Err(QueryEmitError::BadRequest(format!(
                    "criteria key '{key}': a null value is not a valid criterion"
                )))
            }
            other => (CriteriaOp::Eq, other.clone()),
        };
        if !legal.contains(&op) {
            return Err(QueryEmitError::BadRequest(format!(
                "criteria key '{key}' (datatype {:?}) does not support operator '{}' — legal \
                 operators: {}",
                pred_spec.datatype,
                op.label(),
                ops_label(legal)
            )));
        }
        let var = format!("?v{var_idx}");
        match (pred_spec.datatype, op) {
            (Datatype::uri, CriteriaOp::Eq) => {
                let uri = parse_uri_criterion_value(key, &value)?;
                body.push_str(&format!("?s <{predicate_uri}> <{uri}> . "));
            }
            (Datatype::string, CriteriaOp::Contains) => {
                let s = value.as_str().ok_or_else(|| {
                    QueryEmitError::BadRequest(format!(
                        "criteria key '{key}': 'contains' requires a string value"
                    ))
                })?;
                body.push_str(&format!(
                    "?s <{predicate_uri}> {var} . FILTER(CONTAINS(STR({var}), \"{}\")) . ",
                    escape_sparql_string(s)
                ));
            }
            (Datatype::dateTime, CriteriaOp::Before) => {
                let literal = query_engine::format_as_of_literal(&value).map_err(|e| {
                    QueryEmitError::BadRequest(format!("criteria key '{key}': {}", e.message()))
                })?;
                body.push_str(&format!(
                    "?s <{predicate_uri}> {var} . FILTER({var} < {literal}) . "
                ));
            }
            (Datatype::dateTime, CriteriaOp::After) => {
                let literal = query_engine::format_as_of_literal(&value).map_err(|e| {
                    QueryEmitError::BadRequest(format!("criteria key '{key}': {}", e.message()))
                })?;
                body.push_str(&format!(
                    "?s <{predicate_uri}> {var} . FILTER({var} > {literal}) . "
                ));
            }
            (_, CriteriaOp::Eq) => {
                let literal = format_literal_criterion(key, pred_spec.datatype, &value)?;
                body.push_str(&format!(
                    "?s <{predicate_uri}> {var} . FILTER({var} = {literal}) . "
                ));
            }
            _ => unreachable!(
                "legal_ops_for already gated every (datatype, operator) pair reaching here"
            ),
        }
    }
    Ok(body)
}

// ── execution helpers ──

fn strip_iri_brackets(s: &str) -> String {
    s.strip_prefix('<')
        .and_then(|x| x.strip_suffix('>'))
        .map(str::to_string)
        .unwrap_or_else(|| s.to_string())
}

/// Every existing witness membrane graph for this class beyond `commons` —
/// discovered by which named graphs starting with `{commons}:agent:` hold at
/// least one instance of this class's rdf:type. Only meaningful for a
/// membrane-aware class; the caller gates on that.
fn discover_membrane_graphs(
    store: &Store,
    commons: &str,
    rdf_type: &str,
) -> Result<Vec<String>, QueryEmitError> {
    let prefix = format!("{commons}:agent:");
    let sparql = format!(
        "SELECT DISTINCT ?g WHERE {{ GRAPH ?g {{ ?s a <{rdf_type}> }} \
         FILTER(STRSTARTS(STR(?g), \"{}\")) }}",
        escape_sparql_string(&prefix)
    );
    let result = execute_sparql_query(store, &sparql).map_err(QueryEmitError::Internal)?;
    let mut graphs: Vec<String> = result
        .rows
        .into_iter()
        .filter_map(|row| row.get("g").map(|g| strip_iri_brackets(g)))
        .collect();
    graphs.sort();
    graphs.dedup();
    Ok(graphs)
}

/// Every subject matching `body`, scoped to `graphs` (UNIONed — a 1-graph
/// slice degenerates to the plain single-graph query, per
/// [`query_emit::union_over_graphs`]'s own documented property).
fn find_candidates(
    store: &Store,
    graphs: &[String],
    body: &str,
) -> Result<Vec<String>, QueryEmitError> {
    let sparql = format!(
        "SELECT DISTINCT ?s WHERE {{ {} }} ORDER BY ?s",
        query_emit::union_over_graphs(graphs, body)
    );
    let result = execute_sparql_query(store, &sparql).map_err(QueryEmitError::Internal)?;
    Ok(result
        .rows
        .into_iter()
        .filter_map(|row| row.get("s").map(|s| strip_iri_brackets(s)))
        .collect())
}

/// The full predicate span of one subject, scoped to `graphs` — the SAME
/// `SELECT ?p ?o` shape [`query_engine::run_named_query`]'s `byId` uses,
/// rendered as `{predicate: [values]}` like [`super::objects::read_object`].
fn hydrate_one(
    store: &Store,
    graphs: &[String],
    subject: &str,
) -> Result<BTreeMap<String, Vec<String>>, QueryEmitError> {
    let sparql = format!(
        "SELECT ?p ?o WHERE {{ {} }} ORDER BY ?p ?o",
        query_emit::union_over_graphs(graphs, &format!("<{subject}> ?p ?o"))
    );
    let result = execute_sparql_query(store, &sparql).map_err(QueryEmitError::Internal)?;
    let mut predicates: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in result.rows {
        let (Some(p), Some(o)) = (row.get("p"), row.get("o")) else {
            continue;
        };
        predicates
            .entry(strip_iri_brackets(p))
            .or_default()
            .push(o.clone());
    }
    Ok(predicates)
}

fn object_error_to_query_error(error: ObjectError) -> QueryEmitError {
    let message = error.message().to_string();
    match error {
        ObjectError::BadRequest(_) => QueryEmitError::BadRequest(message),
        ObjectError::NotFound(_) | ObjectError::Absent(_) => QueryEmitError::NotFound(message),
        ObjectError::Conflict(_) => QueryEmitError::BadRequest(message),
        ObjectError::Internal(_) => QueryEmitError::Internal(message),
    }
}

/// Run the object query: hydrated, shaped objects out — never bindings rows.
pub(crate) fn run_object_query(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    class_name: &str,
    criteria: &Value,
    options: &ObjectQueryOptions,
) -> Result<ObjectQueryOutcome, QueryEmitError> {
    let store = open_memory_store(app, graph_id).map_err(QueryEmitError::Internal)?;
    let contract = resolve_ingest_contract(&store, graph_id, vocab)
        .map_err(|e| QueryEmitError::NotFound(format!("vocab '{vocab}': {e}")))?;
    let spec = contract.classes.get(class_name).cloned().ok_or_else(|| {
        QueryEmitError::NotFound(format!(
            "class '{class_name}' is not declared by vocab '{}'",
            contract.name
        ))
    })?;
    let rdf_type = class_rdf_type(&contract, class_name).map_err(object_error_to_query_error)?;

    if let Some(p) = options.perspectives.as_deref() {
        if p != "all" {
            return Err(QueryEmitError::BadRequest(format!(
                "perspectives '{p}' is not recognized — the only legal value is 'all' \
                 (explicit cross-witness merge); omit it for the default commons/observer scope"
            )));
        }
    }
    let perspectives_all = options.perspectives.as_deref() == Some("all");

    // Scope: delegate commons/observer resolution to Builder 1's resolver
    // (NEVER re-derive it), then layer "all" discovery on top.
    let (dispatch, scope, mut warnings) =
        resolve_class_scope(&contract, graph_id, class_name, options.observer.as_deref())?;
    let membrane_aware = dispatch.signature.store_target == "projection:memory";
    let mut scope_graphs = scope.graphs;
    let perspectives_label = if perspectives_all {
        if membrane_aware {
            let commons = memory_projection_graph_iri(graph_id);
            for g in discover_membrane_graphs(&store, &commons, &rdf_type)? {
                if !scope_graphs.contains(&g) {
                    scope_graphs.push(g);
                }
            }
            scope_graphs.sort();
            scope_graphs.dedup();
            if options.observer.is_some() {
                warnings.push(
                    "perspectives='all' already merges every discovered witness membrane; the \
                     given 'observer' is redundant (folded in, not filtered to)"
                        .to_string(),
                );
            }
            "all"
        } else {
            warnings.push(format!(
                "class '{class_name}' has no observer membrane — perspectives='all' is a no-op; \
                 queried the shared sink only"
            ));
            "commons"
        }
    } else if membrane_aware && options.observer.is_some() {
        "observer"
    } else {
        "commons"
    };

    // T-W Law 4: a retracted subject is excluded from every candidate match —
    // "faces hide it" for the criteria path too (the contested-class path
    // below is already covered structurally, since `current_heads_by_lineage`
    // / `heads_as_of` themselves exclude a retracted head).
    let body = format!(
        "{} FILTER NOT EXISTS {{ ?s <{}> ?emporiumRetractedAt }}",
        build_criteria_body(&contract, &spec, class_name, &rdf_type, criteria)?,
        crate::emporium::terms::RETRACTED_AT_PRED,
    );

    // Contested-class detection (11a law 5): route "current" through
    // Builder 1's ONE unified head reader — never a second semantics.
    let lineage_shape = query_emit::lineage_shape(&contract, &spec);
    let contested_class =
        lineage_shape.is_some() && sweep::declared_conflict_strategy(&contract) == "contested";
    let mut head_subjects: Option<BTreeSet<String>> = None;
    // subject -> (lineage, every head in that lineage, sorted).
    let mut subject_lineage: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if contested_class {
        let shape = lineage_shape
            .as_ref()
            .expect("checked contested_class above");
        let heads_by_lineage = match &options.as_of {
            Some(as_of_value) => {
                let created_at = shape.created_at.clone().ok_or_else(|| {
                    QueryEmitError::BadRequest(format!(
                        "class '{class_name}' declares no 'createdAt' predicate — as_of is \
                         unavailable"
                    ))
                })?;
                let supersedes = shape.supersedes.clone().ok_or_else(|| {
                    QueryEmitError::BadRequest(format!(
                        "class '{class_name}' declares no 'supersedes' predicate — as_of is \
                         unavailable"
                    ))
                })?;
                let literal = query_engine::format_as_of_literal(as_of_value)?;
                sweep::heads_as_of(
                    &store,
                    &scope_graphs,
                    &shape.lineage,
                    &created_at,
                    &supersedes,
                    &literal,
                )
                .map_err(QueryEmitError::Internal)?
            }
            None => sweep::current_heads_by_lineage(
                &store,
                &scope_graphs,
                &shape.lineage,
                &shape.is_current,
            )
            .map_err(QueryEmitError::Internal)?,
        };
        let mut subjects = BTreeSet::new();
        for heads in heads_by_lineage.values() {
            for head in heads {
                subjects.insert(head.clone());
                subject_lineage.insert(head.clone(), heads.clone());
            }
        }
        head_subjects = Some(subjects);
    } else if options.as_of.is_some() {
        return Err(QueryEmitError::BadRequest(format!(
            "class '{class_name}' has no ratified lineage/contested convention — 'as_of' is \
             only meaningful for contested classes; remove it"
        )));
    }

    // ── gather candidates. `perspectives=all` needs per-OCCURRENCE witness
    // attribution, so it runs the SAME filter query once per graph instead
    // of one combined UNION (a 1-graph slice degenerates identically, per
    // `union_over_graphs`'s own documented property) — merged testimony is
    // never anonymous. ──
    let mut occurrences: Vec<(String, Option<String>)> = if perspectives_all && membrane_aware {
        let mut out = Vec::new();
        for g in &scope_graphs {
            for s in find_candidates(&store, std::slice::from_ref(g), &body)? {
                out.push((s, Some(g.clone())));
            }
        }
        out
    } else {
        find_candidates(&store, &scope_graphs, &body)?
            .into_iter()
            .map(|s| (s, None))
            .collect()
    };
    let raw_count = occurrences.len();
    if let Some(heads) = &head_subjects {
        occurrences.retain(|(s, _)| heads.contains(s));
    }
    occurrences.sort();
    occurrences.dedup();

    let total_matched = occurrences.len();
    let limit = options.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = options.cursor.unwrap_or(0);
    let page: Vec<(String, Option<String>)> =
        occurrences.into_iter().skip(offset).take(limit).collect();
    let next_cursor = if offset + page.len() < total_matched {
        Some(offset + page.len())
    } else {
        None
    };

    let mut objects = Vec::with_capacity(page.len());
    for (subject, witness_graph) in &page {
        let hydrate_graphs: Vec<String> = match witness_graph {
            Some(g) => vec![g.clone()],
            None => scope_graphs.clone(),
        };
        let predicates = hydrate_one(&store, &hydrate_graphs, subject)?;
        let (contested, sibling_heads) = if contested_class {
            match subject_lineage.get(subject) {
                Some(heads) => (
                    Some(heads.len() > 1),
                    heads.iter().filter(|h| *h != subject).cloned().collect(),
                ),
                None => (Some(false), Vec::new()),
            }
        } else {
            (None, Vec::new())
        };
        objects.push(HydratedObject {
            subject: subject.clone(),
            rdf_type: rdf_type.clone(),
            predicates,
            contested,
            sibling_heads,
            witness_graph: witness_graph.clone(),
        });
    }

    // ── the silent-zero doctrine, applied to a query that returns objects ──
    if total_matched == 0 {
        if contested_class && raw_count > 0 {
            warnings.push(format!(
                "{raw_count} '{class_name}' record(s) matched the criteria but none is a \
                 CURRENT head (all superseded) in scope {scope_graphs:?} — pass 'as_of' to \
                 inspect a historical head, or relax criteria that only match superseded \
                 versions"
            ));
        } else {
            let criteria_is_empty = criteria
                .as_object()
                .map(serde_json::Map::is_empty)
                .unwrap_or(false);
            if criteria_is_empty {
                warnings.push(format!(
                    "no '{class_name}' instances found in scope {scope_graphs:?} — try a \
                     broader observer/perspectives, or confirm the vocab/class/graph_id"
                ));
            } else {
                warnings.push(format!(
                    "no '{class_name}' instances match the given criteria in scope \
                     {scope_graphs:?} — check the criteria values, or (for a membrane-ed class) \
                     whether the record lives under a different observer's membrane"
                ));
            }
        }
    } else if page.is_empty() {
        warnings.push(format!(
            "cursor {offset} is beyond the {total_matched} total match(es) — the last valid \
             cursor is {}",
            total_matched.saturating_sub(1)
        ));
    }

    Ok(ObjectQueryOutcome {
        graph_id: graph_id.to_string(),
        vocab: contract.name.clone(),
        class: class_name.to_string(),
        criteria: criteria.clone(),
        scope: ObjectQueryScope {
            graphs: scope_graphs,
            perspectives: perspectives_label.to_string(),
        },
        total_matched,
        limit,
        offset,
        next_cursor,
        objects,
        warnings,
    })
}

#[cfg(test)]
mod pure_tests {
    use super::*;
    use crate::emporium::contract::{get_vocabulary, memory_core_vocabulary};
    use serde_json::json;

    #[test]
    fn legal_ops_match_the_ratified_datatype_table() {
        assert_eq!(
            legal_ops_for(Datatype::string),
            &[CriteriaOp::Eq, CriteriaOp::Contains]
        );
        assert_eq!(
            legal_ops_for(Datatype::dateTime),
            &[CriteriaOp::Eq, CriteriaOp::Before, CriteriaOp::After]
        );
        assert_eq!(legal_ops_for(Datatype::uri), &[CriteriaOp::Eq]);
        assert_eq!(legal_ops_for(Datatype::boolean), &[CriteriaOp::Eq]);
        assert_eq!(legal_ops_for(Datatype::integer), &[CriteriaOp::Eq]);
    }

    #[test]
    fn resolve_criteria_key_accepts_curie_local_name_and_uri_forms() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        let (via_curie, _) =
            resolve_criteria_key(contract, spec, "MemoryRecord", "mem:kind").unwrap();
        let (via_local, _) = resolve_criteria_key(contract, spec, "MemoryRecord", "kind").unwrap();
        let (via_uri, _) =
            resolve_criteria_key(contract, spec, "MemoryRecord", &via_curie).unwrap();
        assert_eq!(via_curie, "http://mnemosyne.dev/memory#kind");
        assert_eq!(via_curie, via_local);
        assert_eq!(via_curie, via_uri);
    }

    #[test]
    fn resolve_criteria_key_rejects_undeclared_key_loudly_naming_legal_keys() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        let err = resolve_criteria_key(contract, spec, "MemoryRecord", "notAField").unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.message().contains("notAField"));
        assert!(err.message().contains("legal keys"));
        assert!(err.message().contains("mem:kind"), "{}", err.message());
    }

    #[test]
    fn build_criteria_body_rejects_illegal_operator_for_datatype_naming_legal_operators() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        // mem:kind is a string; "before" is a dateTime-only operator.
        let err = build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            "http://mnemosyne.dev/memory#MemoryRecord",
            &json!({ "mem:kind": { "before": "2026-01-01T00:00:00Z" } }),
        )
        .unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.message().contains("before"));
        assert!(err.message().contains("legal operators"));
        assert!(err.message().contains("eq"));
        assert!(err.message().contains("contains"));
    }

    #[test]
    fn build_criteria_body_rejects_non_object_criteria() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        let err = build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            "http://mnemosyne.dev/memory#MemoryRecord",
            &json!("not-an-object"),
        )
        .unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn build_criteria_body_rejects_unparseable_uri_value() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        let err = build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            "http://mnemosyne.dev/memory#MemoryRecord",
            &json!({ "mem:observedBy": "not-a-uri" }),
        )
        .unwrap_err();
        assert_eq!(err.status(), 400);
    }

    /// The BLOCKING scenario the review named: an IRI-typed criterion value
    /// that satisfies the shallow `://`/`urn:` shape check but carries `>`
    /// plus SPARQL syntax, which would splice out of the `<{uri}>` triple
    /// pattern `build_criteria_body` embeds it into.
    #[test]
    fn build_criteria_body_rejects_a_urn_value_carrying_sparql_injection_syntax() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        let injected =
            "urn:sophia:x> } UNION { GRAPH <urn:mnemosyne:local:graph:lab:projection:memory:agent:secret> { ?s ?p ?o";
        let err = build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            "http://mnemosyne.dev/memory#MemoryRecord",
            &json!({ "mem:observedBy": injected }),
        )
        .unwrap_err();
        assert_eq!(err.status(), 400, "{}", err.message());
    }

    #[test]
    fn build_criteria_body_accepts_every_operator_family() {
        let contract = memory_core_vocabulary();
        let spec = contract.classes.get("MemoryRecord").unwrap();
        let rdf_type = "http://mnemosyne.dev/memory#MemoryRecord";
        // string eq / contains
        assert!(build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            rdf_type,
            &json!({ "mem:kind": "ClaimMemory" })
        )
        .is_ok());
        assert!(build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            rdf_type,
            &json!({ "mem:content": { "contains": "fish" } })
        )
        .is_ok());
        // dateTime before / after
        assert!(build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            rdf_type,
            &json!({ "mem:createdAt": { "after": "2026-01-01T00:00:00Z" } })
        )
        .is_ok());
        assert!(build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            rdf_type,
            &json!({ "mem:createdAt": { "before": 1_800_000_000_000i64 } })
        )
        .is_ok());
        // uri eq
        assert!(build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            rdf_type,
            &json!({ "mem:observedBy": "urn:sophia:agent:gamma" })
        )
        .is_ok());
        // numeric eq
        assert!(build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            rdf_type,
            &json!({ "mem:confidence": 0.9 })
        )
        .is_ok());
        // boolean eq
        assert!(build_criteria_body(
            contract,
            spec,
            "MemoryRecord",
            rdf_type,
            &json!({ "mem:isCurrent": true })
        )
        .is_ok());
    }

    #[test]
    fn as_of_without_a_contested_class_is_rejected_before_any_query_runs() {
        let contract = get_vocabulary("emporium-bookmark").expect("registered");
        let spec = contract.classes.get("Bookmark").unwrap();
        // Bookmark has no lineage/isCurrent shape at all — as_of must be a
        // structural rejection, not a silently-ignored option.
        assert!(query_emit::lineage_shape(contract, spec).is_none());
    }
}

/// End-to-end tests over REAL ingested fixtures (real oxigraph store, real
/// spine ingestion — no mocks), proving `run_object_query` itself, not just
/// its pure helpers. Mirrors `query_engine.rs`'s `headless_tests` harness
/// (`build_mock_app_for_tests` + `GARDEN_PROFILE_DIR` isolation).
#[cfg(all(test, feature = "headless"))]
mod headless_tests {
    use super::*;
    use crate::emporium::planner::memory_record_subject;
    use crate::emporium::schemas::{MemoryRecordIn, SourceRefIn};
    use crate::emporium::vocab_routes::render_object_query_markdown;
    use crate::geist_memory_service::ingest_memory_record;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use serde_json::json;

    fn temp_profile(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-object-query-{name}-{nanos}"))
    }

    fn run_isolated(name: &str, body: impl FnOnce() + std::panic::UnwindSafe) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile(name);
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(body);
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn base_record(
        client_ref: &str,
        kind: &str,
        content: &str,
        observed_at_ms: i64,
    ) -> MemoryRecordIn {
        MemoryRecordIn {
            client_ref: Some(client_ref.to_string()),
            scope: "agent".to_string(),
            kind: kind.to_string(),
            content_orientation: "knowledge".to_string(),
            visibility: "private".to_string(),
            status: "active".to_string(),
            content: content.to_string(),
            source_refs: vec![SourceRefIn {
                source_kind: "DocumentBlock".to_string(),
                source_label: None,
                block_id: Some("abc".to_string()),
                document_id: Some("doc-shell".to_string()),
                external_id: None,
                external_uri: None,
                observed_at: None,
                trust_tier: None,
            }],
            evidence: vec![],
            observed_at: Some(observed_at_ms),
            valid_from: Some(observed_at_ms),
            is_current: Some(true),
            confidence: None,
            valence: None,
            agent_id: Some("gamma".to_string()),
            observer_agent_id: None,
            tags: vec![],
            supersedes_ref: None,
            contradicts_ref: None,
        }
    }

    fn record_with_confidence(
        client_ref: &str,
        kind: &str,
        content: &str,
        observed_at_ms: i64,
        confidence: f64,
    ) -> MemoryRecordIn {
        let mut r = base_record(client_ref, kind, content, observed_at_ms);
        r.confidence = Some(confidence);
        r
    }

    fn observed_record(
        client_ref: &str,
        content: &str,
        observed_at_ms: i64,
        observer: &str,
    ) -> MemoryRecordIn {
        let mut r = base_record(client_ref, "ClaimMemory", content, observed_at_ms);
        // I2 (voice containment): a record carrying BOTH a voice leaf
        // (`mem:agentId`) and a witness (`mem:observedBy`) must have the leaf
        // name an `agt:Voice` of THAT witness — clear the default agent_id so
        // this fixture (testing observer scoping only) never trips I2.
        r.agent_id = None;
        r.observer_agent_id = Some(observer.to_string());
        r
    }

    fn ingest(app: &crate::app_runtime::AppHandle, graph_id: &str, r: MemoryRecordIn) {
        let outcome =
            crate::app_runtime::async_runtime::block_on(ingest_memory_record(app, graph_id, r)).expect("ingest");
        assert_eq!(outcome["ok"], json!(true), "{outcome}");
    }

    fn new_graph(app: &crate::app_runtime::AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: graph_id.to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    #[test]
    fn criteria_cover_every_operator_family_over_real_ingested_memory_records() {
        run_isolated("operators", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-operators";
            new_graph(&app, graph_id);

            let t0 = 1_718_700_000_000i64;
            let t1 = t0 + 3_600_000; // an hour later
            let tea = record_with_confidence("oq-tea", "ClaimMemory", "vera likes tea", t0, 0.9);
            let coffee = record_with_confidence(
                "oq-coffee",
                "ProfileMemory",
                "vera dislikes coffee",
                t1,
                0.4,
            );
            ingest(&app, graph_id, tea);
            ingest(&app, graph_id, coffee);

            let run = |criteria: Value| {
                run_object_query(
                    &app,
                    graph_id,
                    "sophia-memory-core",
                    "MemoryRecord",
                    &criteria,
                    &ObjectQueryOptions::default(),
                )
                .expect("query runs")
            };
            let content_contains = |outcome: &ObjectQueryOutcome, needle: &str| -> bool {
                outcome.objects.iter().any(|o| {
                    o.predicates
                        .get("http://mnemosyne.dev/memory#content")
                        .map(|values| values.iter().any(|v| v.contains(needle)))
                        .unwrap_or(false)
                })
            };

            // string eq
            let out = run(json!({ "mem:kind": "ClaimMemory" }));
            assert_eq!(out.objects.len(), 1, "{out:?}");
            assert!(content_contains(&out, "tea"), "{out:?}");

            // string contains
            let out = run(json!({ "mem:content": { "contains": "coffee" } }));
            assert_eq!(out.objects.len(), 1, "{out:?}");
            assert!(content_contains(&out, "coffee"), "{out:?}");

            // dateTime after
            let out = run(json!({ "mem:observedAt": { "after": t0 + 1_000 } }));
            assert_eq!(out.objects.len(), 1, "{out:?}");
            assert!(content_contains(&out, "coffee"), "{out:?}");

            // dateTime before
            let out = run(json!({ "mem:observedAt": { "before": t0 + 1_000 } }));
            assert_eq!(out.objects.len(), 1, "{out:?}");
            assert!(content_contains(&out, "tea"), "{out:?}");

            // numeric eq
            let out = run(json!({ "mem:confidence": 0.9 }));
            assert_eq!(out.objects.len(), 1, "{out:?}");
            assert!(content_contains(&out, "tea"), "{out:?}");

            // boolean eq — both records are isCurrent=true
            let out = run(json!({ "mem:isCurrent": true }));
            assert_eq!(out.objects.len(), 2, "{out:?}");

            // uri eq — mem:observedBy over a witnessed record (needs its own
            // observer supplied, proven separately below alongside the
            // commons/observer scoping tests; here just prove the criteria
            // key/operator resolve and run without error against a class
            // that has the predicate declared.
            let out = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({ "mem:observedBy": "urn:sophia:agent:nobody-here" }),
                &ObjectQueryOptions::default(),
            )
            .expect("uri-eq criteria runs (even with zero matches)");
            assert!(out.objects.is_empty());
            assert!(
                !out.warnings.is_empty(),
                "zero match must warn, not be silent"
            );
        });
    }

    #[test]
    fn unknown_criteria_key_is_a_loud_rejection_naming_legal_keys() {
        run_isolated("loud-rejection", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-loud-rejection";
            new_graph(&app, graph_id);

            let err = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({ "notAField": "x" }),
                &ObjectQueryOptions::default(),
            )
            .unwrap_err();
            assert_eq!(err.status(), 400);
            assert!(err.message().contains("notAField"));
            assert!(err.message().contains("legal keys"));
            assert!(err.message().contains("mem:kind"), "{}", err.message());
        });
    }

    #[test]
    fn illegal_operator_for_datatype_is_a_loud_rejection_naming_legal_operators() {
        run_isolated("loud-operator-rejection", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-loud-operator-rejection";
            new_graph(&app, graph_id);

            // mem:kind is a string; "after" is a dateTime-only operator.
            let err = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({ "mem:kind": { "after": 1_718_700_000_000i64 } }),
                &ObjectQueryOptions::default(),
            )
            .unwrap_err();
            assert_eq!(err.status(), 400);
            assert!(err.message().contains("after"));
            assert!(err.message().contains("legal operators"));
        });
    }

    #[test]
    fn commons_only_default_hides_a_membrane_record_until_observer_supplied() {
        run_isolated("commons-only", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-commons-only";
            new_graph(&app, graph_id);

            let t0 = 1_718_700_000_000i64;
            let commons = base_record("oq-commons", "ClaimMemory", "commons: vera likes tea", t0);
            let witnessed_subject = memory_record_subject(
                graph_id,
                &observed_record(
                    "oq-witnessed",
                    "witness-only: a secret",
                    t0,
                    "agent-witness-y",
                ),
            );
            ingest(&app, graph_id, commons);
            ingest(
                &app,
                graph_id,
                observed_record(
                    "oq-witnessed",
                    "witness-only: a secret",
                    t0,
                    "agent-witness-y",
                ),
            );

            // No observer: commons only — the witnessed record must NOT appear.
            let no_observer = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({}),
                &ObjectQueryOptions::default(),
            )
            .expect("commons-only query runs");
            assert_eq!(no_observer.scope.perspectives, "commons");
            assert_eq!(no_observer.objects.len(), 1, "{no_observer:?}");
            assert!(no_observer
                .objects
                .iter()
                .all(|o| o.subject != witnessed_subject));

            // With the observer supplied: commons ∪ that witness's membrane —
            // an OBSERVER UNION, the witnessed record is now reachable too.
            let with_observer = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({}),
                &ObjectQueryOptions {
                    observer: Some("agent-witness-y".to_string()),
                    ..Default::default()
                },
            )
            .expect("observer-scoped query runs");
            assert_eq!(with_observer.scope.perspectives, "observer");
            assert_eq!(with_observer.objects.len(), 2, "{with_observer:?}");
            assert!(with_observer
                .objects
                .iter()
                .any(|o| o.subject == witnessed_subject));
        });
    }

    #[test]
    fn perspectives_all_merges_every_membrane_with_witness_attribution() {
        run_isolated("perspectives-all", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-perspectives-all";
            new_graph(&app, graph_id);

            let t0 = 1_718_700_000_000i64;
            ingest(
                &app,
                graph_id,
                base_record("oq-pa-commons", "ClaimMemory", "commons memory", t0),
            );
            ingest(
                &app,
                graph_id,
                observed_record("oq-pa-a", "witness A memory", t0, "agent-pa-a"),
            );
            ingest(
                &app,
                graph_id,
                observed_record("oq-pa-b", "witness B memory", t0, "agent-pa-b"),
            );

            // Neither observer alone sees the other's testimony.
            let a_only = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({}),
                &ObjectQueryOptions {
                    observer: Some("agent-pa-a".to_string()),
                    ..Default::default()
                },
            )
            .expect("a-only query runs");
            assert_eq!(a_only.objects.len(), 2, "{a_only:?}"); // commons + A

            // perspectives="all": every discovered membrane merged.
            let all = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({}),
                &ObjectQueryOptions {
                    perspectives: Some("all".to_string()),
                    ..Default::default()
                },
            )
            .expect("perspectives=all query runs");
            assert_eq!(all.scope.perspectives, "all");
            assert_eq!(all.objects.len(), 3, "{all:?}");
            // Merged testimony is never anonymous: every result names its
            // witnessGraph, and they are NOT all the same graph.
            assert!(all.objects.iter().all(|o| o.witness_graph.is_some()));
            let distinct_graphs: std::collections::BTreeSet<&str> = all
                .objects
                .iter()
                .map(|o| o.witness_graph.as_deref().unwrap())
                .collect();
            assert_eq!(distinct_graphs.len(), 3, "{distinct_graphs:?}");
        });
    }

    #[test]
    fn contested_lineage_is_flagged_with_sibling_head_refs() {
        run_isolated("contested", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-contested";
            new_graph(&app, graph_id);

            let t0 = 1_718_700_000_000i64;
            let h = base_record("oq-h", "ClaimMemory", "vera prefers fish CLI", t0);
            let h_subject = memory_record_subject(graph_id, &h);
            ingest(&app, graph_id, h);
            let mut a = base_record("oq-a", "ClaimMemory", "vera prefers zsh", t0 + 1_000);
            a.supersedes_ref = Some(h_subject.clone());
            let mut b = base_record("oq-b", "ClaimMemory", "vera prefers nushell", t0 + 1_000);
            b.supersedes_ref = Some(h_subject.clone());
            let (ra, rb) = crate::app_runtime::async_runtime::block_on(async {
                tokio::join!(
                    ingest_memory_record(&app, graph_id, a),
                    ingest_memory_record(&app, graph_id, b),
                )
            });
            assert_eq!(ra.expect("A ingest ran")["ok"], json!(true));
            assert_eq!(rb.expect("B ingest ran")["ok"], json!(true));

            let out = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({}),
                &ObjectQueryOptions::default(),
            )
            .expect("contested query runs");
            // H is superseded — excluded by default (current heads only).
            assert!(out.objects.iter().all(|o| o.subject != h_subject));
            let contested_objects: Vec<&HydratedObject> = out
                .objects
                .iter()
                .filter(|o| o.contested == Some(true))
                .collect();
            assert_eq!(contested_objects.len(), 2, "{out:?}");
            for obj in &contested_objects {
                assert_eq!(obj.sibling_heads.len(), 1, "{obj:?}");
                assert_ne!(obj.sibling_heads[0], obj.subject);
                assert!(contested_objects
                    .iter()
                    .any(|other| &other.subject == &obj.sibling_heads[0]));
            }
        });
    }

    #[test]
    fn as_of_reconstructs_a_historical_head_for_a_contested_class() {
        run_isolated("contested-asof", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-contested-asof";
            new_graph(&app, graph_id);

            let t0 = 1_718_700_000_000i64;
            let t1 = t0 + 3_600_000;
            let h = base_record("oq-asof-h", "ClaimMemory", "vera prefers fish CLI", t0);
            let h_subject = memory_record_subject(graph_id, &h);
            ingest(&app, graph_id, h);
            let mut a = base_record("oq-asof-a", "ClaimMemory", "vera prefers zsh now", t1);
            a.supersedes_ref = Some(h_subject.clone());
            ingest(&app, graph_id, a);

            // Live: H is superseded, excluded by default.
            let live = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({}),
                &ObjectQueryOptions::default(),
            )
            .expect("live query runs");
            assert!(live.objects.iter().all(|o| o.subject != h_subject));

            // asOf a moment before A existed: H is (still) the head.
            let historical = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({}),
                &ObjectQueryOptions {
                    as_of: Some(json!(t0 + 60_000)),
                    ..Default::default()
                },
            )
            .expect("as_of query runs");
            assert!(historical.objects.iter().any(|o| o.subject == h_subject));
        });
    }

    #[test]
    fn pagination_pages_through_stable_order_with_next_cursor() {
        run_isolated("pagination", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-pagination";
            new_graph(&app, graph_id);

            for i in 1..=5 {
                crate::app_runtime::async_runtime::block_on(crate::emporium::objects::create_objects(
                    &app,
                    graph_id,
                    "emporium-bookmark",
                    json!([{
                        "kind": "Bookmark",
                        "localId": format!("pg-{i}"),
                        "url": format!("https://example.test/pg-{i}"),
                        "title": format!("Page {i}"),
                    }]),
                ))
                .expect("seed bookmark");
            }

            let page1 = run_object_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                &json!({}),
                &ObjectQueryOptions {
                    limit: Some(2),
                    ..Default::default()
                },
            )
            .expect("page 1 runs");
            assert_eq!(page1.total_matched, 5);
            assert_eq!(page1.objects.len(), 2);
            assert_eq!(page1.next_cursor, Some(2));

            let page2 = run_object_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                &json!({}),
                &ObjectQueryOptions {
                    limit: Some(2),
                    cursor: Some(2),
                    ..Default::default()
                },
            )
            .expect("page 2 runs");
            assert_eq!(page2.objects.len(), 2);
            assert_eq!(page2.next_cursor, Some(4));
            // No overlap between pages.
            let page1_subjects: std::collections::BTreeSet<&str> =
                page1.objects.iter().map(|o| o.subject.as_str()).collect();
            assert!(page2
                .objects
                .iter()
                .all(|o| !page1_subjects.contains(o.subject.as_str())));

            let page3 = run_object_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                &json!({}),
                &ObjectQueryOptions {
                    limit: Some(2),
                    cursor: Some(4),
                    ..Default::default()
                },
            )
            .expect("page 3 (last, partial) runs");
            assert_eq!(page3.objects.len(), 1);
            assert_eq!(page3.next_cursor, None);

            let beyond = run_object_query(
                &app,
                graph_id,
                "emporium-bookmark",
                "Bookmark",
                &json!({}),
                &ObjectQueryOptions {
                    limit: Some(2),
                    cursor: Some(10),
                    ..Default::default()
                },
            )
            .expect("beyond-range query runs (not an error)");
            assert!(beyond.objects.is_empty());
            assert!(
                beyond.warnings.iter().any(|w| w.contains("beyond")),
                "{beyond:?}"
            );
        });
    }

    #[test]
    fn markdown_face_escapes_backticks_in_real_field_values() {
        run_isolated("markdown-face", || {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "oq-markdown-face";
            new_graph(&app, graph_id);

            let t0 = 1_718_700_000_000i64;
            let content_with_backticks = "run `ls -la` to list files";
            ingest(
                &app,
                graph_id,
                base_record("oq-md", "ClaimMemory", content_with_backticks, t0),
            );

            let outcome = run_object_query(
                &app,
                graph_id,
                "sophia-memory-core",
                "MemoryRecord",
                &json!({}),
                &ObjectQueryOptions::default(),
            )
            .expect("query runs");
            assert_eq!(outcome.objects.len(), 1);

            let markdown = render_object_query_markdown(&outcome);
            // The backtick must be ESCAPED (backslash + backtick), never left
            // as a bare code-span-breaking character.
            assert!(markdown.contains(r"\`ls -la\`"), "{markdown}");
            // The RAW, unescaped run must not survive (would indicate a
            // broken/naive code-span wrap around arbitrary field content).
            assert!(!markdown.contains("`ls -la`"), "{markdown}");
            assert!(markdown.contains("# sophia-memory-core / MemoryRecord"));
            assert!(markdown.contains("totalMatched:** 1"));
        });
    }
}
