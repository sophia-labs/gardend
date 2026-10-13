//! `datasheet_facts { part | artifact, parameter? }`: the chat and the Writer's
//! read of the light records `datasheet_import` wrote
//! (`plans/2026-10-08-datasheets-in-sophia.md` section 3, "Chat and the Writer").
//!
//! Per claim it returns the value, its regime and conditions (row and
//! per-slot), the page and box, every reading and the corroboration, the
//! diagnostics, people's checks (imported snapshots today; the reader's Check
//! verb writes native ones in a later slice) and a deep link. A parameter that
//! was not found is absent from the answer: *not found*, never zero.

use std::collections::BTreeMap;

use oxigraph::model::{GraphNameRef, NamedNode, NamedOrBlankNode, Term};
use oxigraph::store::Store;
use serde_json::{json, Value};

use crate::app_error::{AppError, AppResult};
use crate::app_runtime::AppHandle;
use crate::datasheet_import::{de, eng, norm_ident, Obj, DE, ENG, EP};
use crate::mcp_arg_utils::{mcp_arg_string, mcp_required_graph_id};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const DEFAULT_LIMIT: usize = 200;

type Props = BTreeMap<String, Vec<Obj>>;

struct View {
    graph: String,
    subjects: BTreeMap<String, Props>,
}

pub(crate) fn obj_of(term: &Term) -> Obj {
    match term {
        Term::NamedNode(n) => Obj::Iri(n.as_str().to_string()),
        Term::Literal(l) => {
            if l.datatype().as_str() == XSD_STRING || l.language().is_some() {
                Obj::Text(l.value().to_string())
            } else {
                Obj::Typed(l.value().to_string(), l.datatype().as_str().to_string())
            }
        }
        other => Obj::Text(other.to_string()),
    }
}

fn load_view(store: &Store, graph: &NamedNode) -> Result<View, String> {
    let mut subjects: BTreeMap<String, Props> = BTreeMap::new();
    for quad in store.quads_for_pattern(None, None, None, Some(GraphNameRef::NamedNode(graph.as_ref()))) {
        let quad = quad.map_err(|e| e.to_string())?;
        let NamedOrBlankNode::NamedNode(s) = quad.subject else {
            continue;
        };
        subjects
            .entry(s.into_string())
            .or_default()
            .entry(quad.predicate.into_string())
            .or_default()
            .push(obj_of(&quad.object));
    }
    Ok(View {
        graph: graph.as_str().to_string(),
        subjects,
    })
}

fn text(p: &Props, pred: &str) -> Option<String> {
    p.get(pred)?.iter().find_map(|o| match o {
        Obj::Text(t) => Some(t.clone()),
        Obj::Typed(t, _) => Some(t.clone()),
        Obj::Iri(_) => None,
    })
}
fn texts(p: &Props, pred: &str) -> Vec<String> {
    p.get(pred)
        .map(|v| {
            v.iter()
                .filter_map(|o| match o {
                    Obj::Text(t) | Obj::Typed(t, _) => Some(t.clone()),
                    Obj::Iri(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}
fn iris(p: &Props, pred: &str) -> Vec<String> {
    p.get(pred)
        .map(|v| {
            v.iter()
                .filter_map(|o| match o {
                    Obj::Iri(i) => Some(i.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}
fn num(p: &Props, pred: &str) -> Option<f64> {
    text(p, pred)?.parse().ok()
}
fn uint(p: &Props, pred: &str) -> Option<u64> {
    text(p, pred)?.parse().ok()
}
fn local(iri: &str) -> String {
    iri.rsplit(['#', '/']).next().unwrap_or(iri).to_string()
}
fn has_type(p: &Props, class: &str) -> bool {
    p.get(RDF_TYPE)
        .is_some_and(|v| v.iter().any(|o| *o == Obj::Iri(class.to_string())))
}

fn parse_rects(lexical: &str) -> Vec<Vec<f64>> {
    lexical
        .split(';')
        .map(|r| r.split_whitespace().filter_map(|n| n.parse().ok()).collect::<Vec<f64>>())
        .filter(|r| r.len() == 4)
        .collect()
}

pub(crate) struct FactsQuery {
    pub(crate) part: Option<String>,
    pub(crate) artifact: Option<String>,
    pub(crate) parameter: Option<String>,
    pub(crate) limit: usize,
}

fn agrees(claim: (Option<f64>, Option<f64>), reading: (Option<f64>, Option<f64>)) -> bool {
    let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0),
        (None, None) => true,
        // A reading that states only one side agrees on the side it states.
        _ => false,
    };
    close(claim.0, reading.0) && close(claim.1, reading.1)
}

fn record_id(iri: &str) -> String {
    iri.split_once(":record:").map_or(iri.to_string(), |(_, r)| r.to_string())
}

struct EditionHit<'a> {
    view: &'a View,
    edition: &'a Props,
    families: Vec<String>,
    artifact: Option<String>,
}

fn edition_of(view: &View) -> Option<EditionHit<'_>> {
    let (_, edition) = view
        .subjects
        .iter()
        .find(|(_, p)| has_type(p, &de("DocumentEdition")))?;
    let families = iris(edition, &de("namesFamily"))
        .iter()
        .filter_map(|f| view.subjects.get(f))
        .filter_map(|p| text(p, &eng("normalizedIdentifier")))
        .collect();
    let artifact = view
        .subjects
        .values()
        .find(|p| has_type(p, &de("FileSourceBinding")))
        .and_then(|p| text(p, &de("artifact")));
    Some(EditionHit {
        view,
        edition,
        families,
        artifact,
    })
}

fn claim_matches_parameter(p: &Props, wanted: &str) -> bool {
    let w = norm_ident(wanted);
    if w.is_empty() {
        return true;
    }
    let candidates = [
        text(p, &eng("parameterKey")),
        text(p, &eng("symbolLexical")),
        text(p, &eng("parameterLexical")),
        iris(p, &de("predicate")).first().map(|i| local(i)),
    ];
    candidates
        .iter()
        .flatten()
        .map(|c| norm_ident(c))
        .any(|c| c == w || (w.len() >= 3 && c.contains(&w)))
}

fn slot_json(p: &Props, slot: &str) -> Value {
    json!({
        "conditions": text(p, &eng(&format!("{slot}Conditions"))),
        "guarantee": text(p, &eng(&format!("{slot}Guarantee"))),
    })
}

fn claim_json(
    hit: &EditionHit<'_>,
    iri: &str,
    p: &Props,
    kind: &str,
    readings: &[(&String, &Props)],
    checks: &[(&String, &Props)],
) -> Value {
    let id = record_id(iri);
    let lower = num(p, &eng("lowerLimit"));
    let typical = num(p, &eng("typical"));
    let upper = num(p, &eng("upperLimit"));
    let regime = iris(p, &eng("limitRegime")).first().map(|i| local(i));
    let page_index = uint(p, &de("pageIndex"));
    let rects = text(p, &de("rectangles")).map(|r| parse_rects(&r)).unwrap_or_default();
    let mut sources: Vec<String> = Vec::new();
    let readings_json: Vec<Value> = readings
        .iter()
        .map(|(riri, r)| {
            let role = iris(r, &de("sourceRole")).first().map(|i| local(i));
            let rl = num(r, &eng("lowerLimit"));
            let ru = num(r, &eng("upperLimit"));
            if let Some(role) = &role {
                if agrees((lower, upper), (rl, ru)) && !sources.contains(role) {
                    sources.push(role.clone());
                }
            }
            json!({
                "id": record_id(riri),
                "sourceRole": role,
                "readerTools": iris(r, &de("readerTool")).iter().map(|i| local(i)).collect::<Vec<_>>(),
                "page": uint(r, &de("pageIndex")).map(|i| i + 1),
                "lower": rl, "upper": ru,
                "externalFile": text(r, &de("externalFile")),
                "diagnostics": texts(r, &de("diagnostic")),
            })
        })
        .collect();
    // Tom's level (the cards' `confidence` may say "contested" instead).
    let mut levels = iris(p, &de("upstreamConfidence"));
    levels.extend(iris(p, &de("confidence")));
    let confidence = levels.first().map(|i| local(i));
    let (independent, source_list) = if kind == "keyParameter" {
        (sources.len(), sources.clone())
    } else {
        (1, vec!["specTable".to_string()])
    };
    let checks_json: Vec<Value> = checks
        .iter()
        .map(|(_, a)| {
            json!({
                "facet": iris(a, &de("assessmentFacet")).first().map(|i| local(i)),
                "result": iris(a, &de("assessmentResult")).first().map(|i| local(i)),
                "by": text(a, &de("assessorLabel")),
                "on": text(a, &de("assessedOn")).or_else(|| text(a, &de("assessedOnLexical"))),
                "evidence": text(a, &de("evidenceText")),
                "snapshot": {"lower": num(a, &de("snapshotLower")), "upper": num(a, &de("snapshotUpper"))},
                "changedSinceChecked": text(a, &de("changedSinceChecked")).as_deref() == Some("true"),
                "extracted": {"lower": num(a, &de("extractedLower")), "upper": num(a, &de("extractedUpper"))},
                "imported": text(a, &de("imported")).as_deref() == Some("true"),
            })
        })
        .collect();
    let checked = checks_json
        .iter()
        .any(|c| c["result"] == json!("accept") && c["changedSinceChecked"] == json!(false));
    json!({
        "id": id,
        "kind": kind,
        "parameter": text(p, &eng("parameterKey")).or_else(|| text(p, &eng("parameterLexical"))),
        "symbol": text(p, &eng("symbolLexical")),
        "section": text(p, &eng("section")),
        "group": text(p, &eng("group")),
        "predicate": iris(p, &de("predicate")).first().cloned(),
        "value": {
            "lower": lower, "typical": typical, "upper": upper,
            "unit": text(p, &eng("unitLexical")),
            "text": text(p, &de("valueText")),
        },
        "regime": regime,
        "conditions": {
            "row": text(p, &de("conditions")),
            "completeness": iris(p, &de("conditionsCompleteness")).first().map(|i| local(i)),
            "slots": {
                "lower": slot_json(p, "lower"),
                "typical": slot_json(p, "typical"),
                "upper": slot_json(p, "upper"),
            },
        },
        "page": page_index.map(|i| i + 1),
        "box": if rects.is_empty() { Value::Null } else { json!(rects) },
        "precision": iris(p, &de("precision")).first().map(|i| local(i)),
        "geometryStatus": iris(p, &de("geometryStatus")).first().map(|i| local(i)),
        "readings": readings_json,
        "corroboration": {
            "level": confidence,
            "independentSources": independent,
            "sources": source_list,
            "contested": text(p, &de("contested")).as_deref() == Some("true"),
        },
        "diagnostics": texts(p, &de("diagnostic")),
        "checks": checks_json,
        "checkedByPerson": checked,
        "link": hit.artifact.as_ref().map(|a| format!("/files/{a}#evidence={id}")),
    })
}

fn edition_json(hit: &EditionHit<'_>, q: &FactsQuery) -> Value {
    let view = hit.view;
    let mut checks_by_target: BTreeMap<String, Vec<(&String, &Props)>> = BTreeMap::new();
    for (iri, p) in &view.subjects {
        if has_type(p, &de("Assessment")) {
            for target in iris(p, &de("assesses")) {
                checks_by_target.entry(target).or_default().push((iri, p));
            }
        }
    }
    let mut keyed: Vec<(u64, &String, &Props)> = Vec::new();
    let mut rows: Vec<(u64, &String, &Props)> = Vec::new();
    let mut corrections: Vec<Value> = Vec::new();
    for (iri, p) in &view.subjects {
        if !has_type(p, &de("Assertion")) {
            continue;
        }
        let kind = iris(p, &de("assertionKind")).first().map(|i| local(i)).unwrap_or_default();
        let ord = uint(p, &de("ordinal")).unwrap_or(0);
        match kind.as_str() {
            "derivation" => keyed.push((ord, iri, p)),
            "sourceReport" if text(p, &eng("section")).is_some() => rows.push((ord, iri, p)),
            "correction" => corrections.push(json!({
                "field": iris(p, &de("predicate")).first().map(|i| local(i).trim_start_matches("field-").to_string()),
                "value": text(p, &de("value")),
                "imported": true,
            })),
            _ => {}
        }
    }
    keyed.sort();
    rows.sort();
    let mut claims: Vec<Value> = Vec::new();
    let mut truncated = false;
    for (kind, list) in [("keyParameter", &keyed), ("specRow", &rows)] {
        for (_, iri, p) in list.iter() {
            if let Some(w) = &q.parameter {
                if !claim_matches_parameter(p, w) {
                    continue;
                }
            }
            if claims.len() >= q.limit {
                truncated = true;
                continue;
            }
            let readings: Vec<(&String, &Props)> = if kind == "keyParameter" {
                let mut r: Vec<(u64, &String, &Props)> = iris(p, &de("groundingRecords"))
                    .iter()
                    .filter_map(|g| view.subjects.get_key_value(g))
                    .map(|(k, v)| (uint(v, &de("ordinal")).unwrap_or(0), k, v))
                    .collect();
                r.sort();
                r.into_iter().map(|(_, k, v)| (k, v)).collect()
            } else {
                Vec::new()
            };
            let checks = checks_by_target.get(*iri).cloned().unwrap_or_default();
            claims.push(claim_json(hit, iri, p, kind, &readings, &checks));
        }
    }
    let content = view
        .subjects
        .values()
        .find(|p| has_type(p, &de("ByteContent")));
    // How the document was read: the reader tools (name -> commit or version) and which tool read
    // which layer, from the import's ExtractionAttempt (`de:tool`, `de:usedTool`, `de:readers`).
    let attempt = view.subjects.values().find(|p| has_type(p, &de("ExtractionAttempt")));
    let mut tool_versions = serde_json::Map::new();
    for p in view.subjects.values().filter(|p| has_type(p, &de("ToolProfile"))) {
        if let Some(name) = text(p, &de("toolName")) {
            let v = text(p, &de("toolVersion")).or_else(|| text(p, &de("toolCommit")));
            tool_versions.insert(name, v.map_or(Value::Null, Value::String));
        }
    }
    let parse_json = |t: Option<String>| t.and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let reader = attempt.map(|a| {
        json!({
            "tools": Value::Object(tool_versions.clone()),
            "readers": parse_json(text(a, &de("readers"))),
            "generated": text(a, &de("generated")).or_else(|| text(a, &de("generatedLexical"))),
            "doclingRows": uint(a, &de("doclingRows")),
            "pdfplumberSections": uint(a, &de("pdfplumberSections")),
            "unitRepairs": uint(a, &de("unitRepairs")),
            "textDisagreementPages": parse_json(text(a, &de("textDisagreementPages"))),
        })
    });
    json!({
        "reader": reader,
        "makerBy": text(hit.edition, &de("makerBy")),
        "partsBy": text(hit.edition, &de("partsBy")),
        "identity": parse_json(text(hit.edition, &de("identityWitnesses"))),
        "literature": text(hit.edition, &de("literature")),
        "title": text(hit.edition, &de("title")),
        "revision": text(hit.edition, &de("revision")),
        "maker": text(hit.edition, &de("maker")),
        "documentType": text(hit.edition, &de("documentType")),
        "parts": hit.families,
        "artifact": hit.artifact,
        "sha256": content.and_then(|c| text(c, &de("declaredSha256"))),
        "byteCheck": content.and_then(|c| iris(c, &de("byteCheck")).first().map(|i| local(i))),
        "pageCount": uint(hit.edition, &de("pageCount")),
        "namedGraph": view.graph,
        "diagnostics": texts(hit.edition, &de("diagnostic")),
        "corrections": corrections,
        "claims": claims,
        "truncated": truncated,
    })
}

pub(crate) fn facts_from_store(store: &Store, graph_id: &str, q: &FactsQuery) -> AppResult<Value> {
    if q.part.is_none() && q.artifact.is_none() {
        return Err(AppError::validation("give a part or an artifact"));
    }
    let prefix = format!(
        "{}:projection:datasheet:ds-",
        crate::rdf::graph_subject(graph_id)
    );
    let mut graphs: Vec<NamedNode> = store
        .named_graphs()
        .filter_map(Result::ok)
        .filter_map(|g| match g {
            NamedOrBlankNode::NamedNode(n) if n.as_str().starts_with(&prefix) => Some(n),
            _ => None,
        })
        .collect();
    graphs.sort();
    let mut views = Vec::new();
    for g in &graphs {
        views.push(load_view(store, g).map_err(AppError::storage)?);
    }
    let hits: Vec<EditionHit<'_>> = views.iter().filter_map(edition_of).collect();
    let wanted_part = q.part.as_deref().map(norm_ident);
    let wanted_artifact = q.artifact.as_deref().map(str::trim).map(str::to_ascii_lowercase);
    let artifact_ok = |h: &EditionHit<'_>| -> bool {
        let Some(a) = &wanted_artifact else { return true };
        if h.artifact.as_deref().map(str::to_ascii_lowercase).as_deref() == Some(a.as_str()) {
            return true;
        }
        h.view.subjects.values().any(|p| {
            texts(p, &de("artifact")).iter().any(|x| x.eq_ignore_ascii_case(a))
                || (a.len() >= 8
                    && a.bytes().all(|b| b.is_ascii_hexdigit())
                    && text(p, &de("declaredSha256")).is_some_and(|s| s.starts_with(a.as_str())))
        })
    };
    let exact: Vec<&EditionHit<'_>> = hits
        .iter()
        .filter(|h| artifact_ok(h))
        .filter(|h| wanted_part.as_ref().map_or(true, |w| h.families.iter().any(|f| f == w)))
        .collect();
    let w = wanted_part.clone().unwrap_or_default();
    // Nothing exact: a family prefix (LM358 finds LM358B), and for a long
    // enough query a part inside an older multi-part identifier.
    let by_prefix: Vec<&EditionHit<'_>> = if exact.is_empty() && !w.is_empty() {
        hits.iter()
            .filter(|h| artifact_ok(h))
            .filter(|h| {
                h.families
                    .iter()
                    .any(|f| f.starts_with(&w) || (w.len() >= 5 && f.contains(&w)))
            })
            .collect()
    } else {
        Vec::new()
    };
    let matched: Vec<&EditionHit<'_>> = if !by_prefix.is_empty() {
        by_prefix
    } else if exact.is_empty() && wanted_part.is_some() {
        // Fall back to the title for a part no family names at all.
        hits.iter()
            .filter(|h| artifact_ok(h))
            .filter(|h| {
                !w.is_empty()
                    && text(h.edition, &de("title")).is_some_and(|t| norm_ident(&t).contains(&w))
            })
            .collect()
    } else {
        exact
    };
    let editions: Vec<Value> = matched.iter().map(|h| edition_json(h, q)).collect();
    let mut out = json!({
        "graphId": graph_id,
        "query": {"part": q.part, "artifact": q.artifact, "parameter": q.parameter},
        "namespaces": {"de": DE, "eng": ENG, "ep": EP},
        "matched": editions.len(),
        "editions": editions,
    });
    if matched.is_empty() {
        out["note"] = json!("not found: no imported datasheet matches; absence here is not zero");
    }
    Ok(out)
}

/// `datasheet_facts { graph_id, part | artifact, parameter?, limit? }`
pub(crate) fn mcp_local_datasheet_facts(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    crate::ids::validate_local_id(&graph_id, "graph_id").map_err(AppError::validation)?;
    let q = FactsQuery {
        part: mcp_arg_string(arguments, &["part"]),
        artifact: mcp_arg_string(arguments, &["artifact", "artifact_id", "artifactId"]),
        parameter: mcp_arg_string(arguments, &["parameter"]),
        limit: arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, 1000)),
    };
    let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id).map_err(AppError::not_found)?;
    let store = crate::rdf_service::open_graph_store(&graph_dir).map_err(AppError::storage)?;
    facts_from_store(&store, &graph_id, &q)
}
