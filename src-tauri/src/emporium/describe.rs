//! `emporium_describe {graphId, iri}` — the long-wished `describe(iri)` (D8.4
//! of plans/2026-10-20-typed-objects): one typed resource, verbalized for an
//! agent or a face.
//!
//! It reads the subject's types and triples in every named graph (`GRAPH
//! ?g`), finds the class contract among this graph's types (a built-in
//! golden, else the graph's own domain ontology, at the version an item was
//! typed with when it says so), and answers
//! `{iri, class:{iri, label, ...}, title, fields:[{key, label, value | nested
//! object}], testimony, stance}`:
//!
//! - **fields** in presentation order, each with its label and value (a list
//!   for a multi field, a nested object `{iri, class, title, fields}` for an
//!   `object` field);
//! - **testimony**: who made it (`wf:producedBy`, the being), the sitting that
//!   handed it back (`wf:sitting`), the run (`wf:partOfRun`) and when
//!   (`prov:generatedAtTime`);
//! - **stance**: `retracted` (an Emporium retraction), `unfit` (a run item that
//!   did not fit its class: its violations, and its fields read from its
//!   `wf:json`), else `read`.
//!
//! An IRI with no known class is not an error: the answer says `known:
//! false` and lists its triples, the generic face's data. Read-only
//! (`rdf.query`).

use oxigraph::model::{NamedNode, Term};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use serde_json::{json, Map, Value};

use crate::app_error::{AppError, AppResult};
use crate::app_runtime::AppHandle;
use crate::emporium::contract::Datatype;
use crate::emporium::memory_applier::open_memory_store;
use crate::emporium::type_catalog::{
    class_label, ordered_fields, resolve_class_iri, resolve_pinned_class, ResolvedClass,
    MAX_NESTING_DEPTH,
};
use crate::mcp_arg_utils::{mcp_arg_string, mcp_required_graph_id};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const DCT_TITLE: &str = "http://purl.org/dc/terms/title";
const WF: &str = "http://mnemosyne.dev/workflow#";
const PROV_GENERATED: &str = "http://www.w3.org/ns/prov#generatedAtTime";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// The triples of one subject: `(graph, predicate, object)`.
type Rows = Vec<(String, String, Term)>;

fn subject_rows(store: &Store, iri: &str, graph: Option<&str>) -> AppResult<Rows> {
    let query = match graph {
        Some(graph) => format!(
            "SELECT ?g ?p ?o WHERE {{ GRAPH ?g {{ <{iri}> ?p ?o }} FILTER(?g = <{graph}>) }}"
        ),
        None => format!("SELECT ?g ?p ?o WHERE {{ GRAPH ?g {{ <{iri}> ?p ?o }} }}"),
    };
    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| AppError::internal(format!("emporium_describe: parse: {e}")))?
        .on_store(store)
        .execute()
        .map_err(|e| AppError::internal(format!("emporium_describe: query: {e}")))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => return Err(AppError::internal("emporium_describe: expected solutions")),
    };
    let mut rows = Rows::new();
    for solution in solutions {
        let solution =
            solution.map_err(|e| AppError::internal(format!("emporium_describe: row: {e}")))?;
        let (Some(Term::NamedNode(g)), Some(Term::NamedNode(p)), Some(o)) =
            (solution.get("g"), solution.get("p"), solution.get("o"))
        else {
            continue;
        };
        rows.push((g.as_str().to_string(), p.as_str().to_string(), o.clone()));
    }
    Ok(rows)
}

fn values<'a>(rows: &'a Rows, predicate: &str) -> Vec<&'a Term> {
    rows.iter()
        .filter(|(_, p, _)| p == predicate)
        .map(|(_, _, o)| o)
        .collect()
}

fn first_text(rows: &Rows, predicate: &str) -> Option<String> {
    values(rows, predicate)
        .into_iter()
        .find_map(|term| match term {
            Term::Literal(literal) => Some(literal.value().to_string()),
            _ => None,
        })
}

fn first_iri(rows: &Rows, predicate: &str) -> Option<String> {
    values(rows, predicate)
        .into_iter()
        .find_map(|term| match term {
            Term::NamedNode(node) => Some(node.as_str().to_string()),
            _ => None,
        })
}

/// A literal as the JSON value its datatype says; an IRI as its string.
fn term_json(term: &Term) -> Value {
    match term {
        Term::NamedNode(node) => json!(node.as_str()),
        Term::BlankNode(node) => json!(format!("_:{}", node.as_str())),
        Term::Literal(literal) => {
            let datatype = literal.datatype().as_str().to_string();
            let text = literal.value();
            let local = datatype.strip_prefix(XSD).unwrap_or_default();
            match local {
                "integer" | "long" | "int" | "short" | "nonNegativeInteger" => text
                    .parse::<i64>()
                    .map(|n| json!(n))
                    .unwrap_or_else(|_| json!(text)),
                "double" | "float" | "decimal" => text
                    .parse::<f64>()
                    .ok()
                    .and_then(|f| serde_json::Number::from_f64(f).map(Value::Number))
                    .unwrap_or_else(|| json!(text)),
                "boolean" => json!(text == "true" || text == "1"),
                _ => json!(text),
            }
        }
        #[allow(unreachable_patterns)]
        other => json!(other.to_string()),
    }
}

fn class_view(resolved: &ResolvedClass) -> Value {
    json!({
        "iri": resolved.iri,
        "name": resolved.class_name,
        "label": resolved.label(),
        "plural": resolved.plural(),
        "vocab": resolved.vocab,
        "version": resolved.version,
        "source": resolved.source.as_str(),
    })
}

/// The class's fields over `rows`, each `{key, label, datatype, value}`
/// (`value` null when absent, a list for a multi field, a nested object for an
/// `object` field, read from the same graph).
fn typed_fields(
    store: &Store,
    resolved: &ResolvedClass,
    class_name: &str,
    rows: &Rows,
    graph: &str,
    depth: usize,
) -> AppResult<Vec<Value>> {
    let contract = &resolved.contract;
    let Some(class) = contract.classes.get(class_name) else {
        return Ok(Vec::new());
    };
    let mut fields = Vec::new();
    for field in ordered_fields(class) {
        let Ok(predicate) = contract.expand(field.curie) else {
            continue;
        };
        let objects = values(rows, &predicate);
        let render = |term: &Term| -> AppResult<Value> {
            match (field.spec.datatype, term) {
                (Datatype::object, Term::NamedNode(child)) if depth < MAX_NESTING_DEPTH => {
                    let nested = field.spec.class.as_deref().unwrap_or_default();
                    nested_object(store, resolved, nested, child.as_str(), graph, depth + 1)
                }
                _ => Ok(term_json(term)),
            }
        };
        let value = if field.spec.multi {
            let mut list = objects
                .iter()
                .map(|term| render(term))
                .collect::<AppResult<Vec<_>>>()?;
            // Nested list members come back in their `/<key>/<i>` order.
            if field.spec.datatype == Datatype::object {
                list.sort_by_key(|item| {
                    item["iri"]
                        .as_str()
                        .and_then(|iri| iri.rsplit('/').next())
                        .and_then(|i| i.parse::<u64>().ok())
                        .unwrap_or(u64::MAX)
                });
            }
            Value::Array(list)
        } else {
            match objects.first() {
                Some(term) => render(term)?,
                None => Value::Null,
            }
        };
        fields.push(json!({
            "key": field.key,
            "label": field.label(),
            "datatype": field.spec.datatype.token(),
            "value": value,
        }));
    }
    Ok(fields)
}

fn nested_object(
    store: &Store,
    resolved: &ResolvedClass,
    class_name: &str,
    iri: &str,
    graph: &str,
    depth: usize,
) -> AppResult<Value> {
    let rows = subject_rows(store, iri, Some(graph))?;
    let fields = typed_fields(store, resolved, class_name, &rows, graph, depth)?;
    let nested = ResolvedClass {
        class_name: class_name.to_string(),
        iri: crate::emporium::type_catalog::class_iri(&resolved.contract, class_name)
            .unwrap_or_default(),
        ..resolved.clone()
    };
    Ok(json!({
        "iri": iri,
        "class": { "iri": nested.iri, "name": class_name, "label": class_label(&resolved.contract, class_name) },
        "title": title_of(&nested, &fields, &rows),
        "fields": fields,
    }))
}

/// The presentation's title field, else a label triple, else the first text
/// field that has a value.
fn title_of(resolved: &ResolvedClass, fields: &[Value], rows: &Rows) -> Value {
    let class = resolved.contract.classes.get(&resolved.class_name);
    let wanted = class
        .and_then(|class| class.presentation.as_ref())
        .and_then(|p| p.title.clone());
    let text = |value: &Value| match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    if let Some(key) = wanted {
        if let Some(value) = fields
            .iter()
            .find(|f| f["key"] == key.as_str())
            .and_then(|f| text(&f["value"]))
        {
            return json!(value);
        }
    }
    if let Some(label) = first_text(rows, RDFS_LABEL).or_else(|| first_text(rows, DCT_TITLE)) {
        return json!(label);
    }
    fields
        .iter()
        .filter(|f| f["datatype"] == "string")
        .find_map(|f| text(&f["value"]))
        .map(Value::String)
        .unwrap_or(Value::Null)
}

fn subtitle_of(resolved: &ResolvedClass, fields: &[Value]) -> Value {
    resolved
        .contract
        .classes
        .get(&resolved.class_name)
        .and_then(|class| class.presentation.as_ref())
        .and_then(|p| p.subtitle.clone())
        .and_then(|key| {
            fields
                .iter()
                .find(|f| f["key"] == key.as_str())
                .map(|f| f["value"].clone())
        })
        .filter(|value| !value.is_null())
        .unwrap_or(Value::Null)
}

/// Who made it, in which sitting, in which run, when.
fn testimony(store: &Store, rows: &Rows) -> AppResult<Value> {
    let mut testimony = Map::new();
    if let Some(agent) = first_iri(rows, &format!("{WF}producedBy")) {
        let label = subject_rows(store, &agent, None)?
            .iter()
            .find(|(_, p, _)| p == RDFS_LABEL)
            .and_then(|(_, _, o)| match o {
                Term::Literal(l) => Some(l.value().to_string()),
                _ => None,
            });
        testimony.insert("by".into(), json!({ "agentIri": agent, "label": label }));
    }
    if let Some(sitting) = first_text(rows, &format!("{WF}sitting")) {
        testimony.insert("sitting".into(), json!(sitting));
    }
    if let Some(run) = first_iri(rows, &format!("{WF}partOfRun")) {
        let graph = rows.first().map(|(g, _, _)| g.clone());
        let run_rows = subject_rows(store, &run, graph.as_deref())?;
        testimony.insert(
            "run".into(),
            json!({
                "iri": run,
                "id": first_text(&run_rows, &format!("{WF}runId")),
                "graph": graph,
                "workflow": first_iri(&run_rows, &format!("{WF}workflow")),
            }),
        );
    }
    if let Some(at) = first_text(rows, PROV_GENERATED) {
        testimony.insert("at".into(), json!(at));
    }
    Ok(Value::Object(testimony))
}

/// `emporium_describe {graphId, iri}`.
pub(crate) fn mcp_local_emporium_describe(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let iri = mcp_arg_string(arguments, &["iri", "subject"])
        .map(|iri| {
            iri.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
        .filter(|iri| !iri.is_empty())
        .ok_or_else(|| AppError::validation("iri is required: the absolute IRI of the resource"))?;
    NamedNode::new(iri.as_str())
        .map_err(|error| AppError::validation(format!("iri {iri:?} is not an IRI: {error}")))?;
    let store = open_memory_store(&app, &graph_id).map_err(AppError::not_found)?;
    describe(&store, &graph_id, &iri)
}

/// The describe read over an open store (the handler and the tests share it).
pub(crate) fn describe(store: &Store, graph_id: &str, iri: &str) -> AppResult<Value> {
    let rows = subject_rows(store, iri, None)?;
    let mut graphs: Vec<String> = rows.iter().map(|(g, _, _)| g.clone()).collect();
    graphs.sort();
    graphs.dedup();
    let types: Vec<String> = values(&rows, RDF_TYPE)
        .into_iter()
        .filter_map(|t| match t {
            Term::NamedNode(n) => Some(n.as_str().to_string()),
            _ => None,
        })
        .collect();
    let unfit = types.iter().any(|t| t == &format!("{WF}Unfit"));
    let retracted = !values(&rows, crate::emporium::terms::RETRACTED_AT_PRED).is_empty();
    let pin = first_text(&rows, &format!("{WF}itemVocab"));

    // The class: a type the graph's types know (at the pinned version when the
    // item names one), or for an unfit item the class it was meant to be.
    let mut candidates: Vec<String> = types
        .iter()
        .filter(|t| !t.starts_with(WF))
        .cloned()
        .collect();
    if unfit {
        if let Some(meant) = first_iri(&rows, &format!("{WF}itemClass")) {
            candidates.insert(0, meant);
        } else if let Some(step) = first_iri(&rows, &format!("{WF}step")) {
            // Choreograph's own unfit record (`<run#unfit-k> a wf:Unfit ;
            // wf:step <step>`): the class its step names in the plan.
            let step_rows = subject_rows(store, &step, None)?;
            if let Some(meant) = first_iri(&step_rows, &format!("{WF}itemClass")) {
                candidates.insert(0, meant);
            }
        }
    }
    let mut resolved: Option<ResolvedClass> = None;
    for candidate in &candidates {
        let found = match pin.as_deref() {
            Some(pin) => resolve_pinned_class(store, graph_id, candidate, Some(pin))
                .map_err(AppError::internal)?
                .or(resolve_class_iri(store, graph_id, candidate, true)
                    .map_err(AppError::internal)?),
            None => {
                resolve_class_iri(store, graph_id, candidate, true).map_err(AppError::internal)?
            }
        };
        if found.is_some() {
            resolved = found;
            break;
        }
    }
    let stance = if retracted {
        "retracted"
    } else if unfit {
        "unfit"
    } else {
        "read"
    };
    let testimony = testimony(store, &rows)?;
    let violations: Vec<String> = values(&rows, &format!("{WF}violation"))
        .into_iter()
        .filter_map(|t| match t {
            Term::Literal(l) => Some(l.value().to_string()),
            _ => None,
        })
        .collect();

    let Some(resolved) = resolved else {
        // Unknown: the generic face's data, never an error.
        let triples: Vec<Value> = rows
            .iter()
            .map(|(g, p, o)| json!({ "graph": g, "predicate": p, "value": term_json(o) }))
            .collect();
        let mut out = json!({
            "graphId": graph_id,
            "iri": iri,
            "known": false,
            "found": !rows.is_empty(),
            "class": Value::Null,
            "types": types,
            "graphs": graphs,
            "title": first_text(&rows, RDFS_LABEL).or_else(|| first_text(&rows, DCT_TITLE)),
            "fields": [],
            "triples": triples,
            "testimony": testimony,
            "stance": stance,
        });
        if unfit {
            out["violations"] = json!(violations);
        }
        return Ok(out);
    };

    let graph = graphs.first().cloned().unwrap_or_default();
    let (fields, extra) = if unfit {
        unfit_fields(&resolved, &rows)
    } else {
        (
            typed_fields(store, &resolved, &resolved.class_name, &rows, &graph, 0)?,
            Vec::new(),
        )
    };
    let mut out = json!({
        "graphId": graph_id,
        "iri": iri,
        "known": true,
        "found": true,
        "class": class_view(&resolved),
        "types": types,
        "graphs": graphs,
        "title": title_of(&resolved, &fields, &rows),
        "subtitle": subtitle_of(&resolved, &fields),
        "fields": fields,
        "testimony": testimony,
        "stance": stance,
    });
    if unfit {
        out["violations"] = json!(violations);
        if !extra.is_empty() {
            out["extra"] = json!(extra);
        }
    }
    Ok(out)
}

/// An unfit item has no field triples: its fields are read from its
/// `wf:json`, labelled by the class it was meant to be; keys the class does
/// not have are `extra`.
fn unfit_fields(resolved: &ResolvedClass, rows: &Rows) -> (Vec<Value>, Vec<Value>) {
    let item: Value = first_text(rows, &format!("{WF}json"))
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or(Value::Null);
    let object = item.as_object().cloned().unwrap_or_default();
    let fields: Vec<_> = ordered_fields(resolved.class());
    let typed = fields
        .iter()
        .map(|field| {
            json!({
                "key": field.key,
                "label": field.label(),
                "datatype": field.spec.datatype.token(),
                "value": object.get(field.key).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    let extra = object
        .iter()
        .filter(|(key, _)| !fields.iter().any(|f| f.key == key.as_str()))
        .map(|(key, value)| json!({ "key": key, "value": value }))
        .collect();
    (typed, extra)
}
