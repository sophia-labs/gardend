//! One request-local named-graph policy for SPARQL, vector search, and traversal.
use crate::swarm_store::{normalize, Snapshot};
use oxigraph::{
    model::{GraphName, Literal, NamedNode, NamedOrBlankNode, Term},
    sparql::{CancellationToken, QueryResults, SparqlEvaluator},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::mpsc,
    time::Duration,
};

type Result<T> = std::result::Result<T, String>;

pub(crate) struct Mask {
    hidden: HashSet<String>,
}

impl Mask {
    pub(crate) fn new(graphs: &[String]) -> Result<Self> {
        for graph in graphs {
            NamedNode::new(graph.clone()).map_err(|e| e.to_string())?;
        }
        Ok(Self {
            hidden: graphs.iter().cloned().collect(),
        })
    }
    pub(crate) fn visible(&self, graph: &str) -> bool {
        !self.hidden.contains(graph)
    }
}

pub(crate) fn query(
    snapshot: &Snapshot,
    mask: &Mask,
    query: &str,
    max_rows: usize,
) -> Result<Value> {
    if query.len() > 100_000 {
        return Err("swarm SPARQL exceeds 100000 bytes".into());
    }
    // Oxigraph's current dependency has no http-client feature. Reject SERVICE
    // explicitly too, preserving the offline contract if a future build enables it.
    if query.to_ascii_uppercase().contains("SERVICE") {
        return Err("swarm SPARQL SERVICE is disabled".into());
    }
    let cancellation = CancellationToken::new();
    let (deadline, receiver) = mpsc::channel::<()>();
    let timer = cancellation.clone();
    std::thread::spawn(move || {
        if matches!(
            receiver.recv_timeout(Duration::from_secs(15)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            timer.cancel();
        }
    });
    let _deadline = deadline; // Drop wakes the timer immediately on success/error.
    let mut prepared = SparqlEvaluator::new()
        .with_cancellation_token(cancellation.clone())
        .parse_query(query)
        .map_err(|e| format!("parse swarm SPARQL: {e}"))?;
    let dataset = prepared.dataset_mut();
    let defaults = if dataset.is_default_dataset() || dataset.default_graph_graphs().is_none() {
        snapshot
            .graphs
            .iter()
            .filter(|g| mask.visible(g.as_str()))
            .cloned()
            .map(GraphName::NamedNode)
            .collect()
    } else {
        dataset
            .default_graph_graphs()
            .unwrap_or_default()
            .iter()
            .filter(|g| match g {
                GraphName::NamedNode(g) => mask.visible(g.as_str()),
                _ => false,
            })
            .cloned()
            .collect()
    };
    let named = match dataset.available_named_graphs() {
        Some(graphs) => graphs
            .iter()
            .filter(|g| match g {
                NamedOrBlankNode::NamedNode(g) => mask.visible(g.as_str()),
                _ => false,
            })
            .cloned()
            .collect(),
        None => snapshot
            .graphs
            .iter()
            .filter(|g| mask.visible(g.as_str()))
            .cloned()
            .map(NamedOrBlankNode::NamedNode)
            .collect(),
    };
    dataset.set_default_graph(defaults);
    dataset.set_available_named_graphs(named);
    let results = prepared
        .on_store(&snapshot.store)
        .execute()
        .map_err(|e| format!("execute swarm SPARQL: {e}"))?;
    let max_rows = max_rows.clamp(1, 50_000);
    match results {
        QueryResults::Solutions(mut solutions) => {
            let variables: Vec<String> = solutions
                .variables()
                .iter()
                .map(|v| v.as_str().to_owned())
                .collect();
            let mut rows = Vec::new();
            let mut truncated = false;
            for solution in &mut solutions {
                let solution = solution.map_err(|e| e.to_string())?;
                if rows.len() == max_rows {
                    truncated = true;
                    cancellation.cancel();
                    break;
                }
                let row: BTreeMap<String, String> = variables
                    .iter()
                    .filter_map(|v| {
                        solution
                            .get(v.as_str())
                            .map(|term| (v.clone(), term.to_string()))
                    })
                    .collect();
                rows.push(row);
            }
            Ok(
                json!({"variables": variables, "rows": rows, "boolean": null,
                      "truncated": truncated, "snapshot": snapshot.generation}),
            )
        }
        QueryResults::Boolean(boolean) => {
            Ok(json!({"variables": [], "rows": [], "boolean": boolean,
                                                   "truncated": false, "snapshot": snapshot.generation}))
        }
        _ => Err("swarm queries support SELECT and ASK only".into()),
    }
}

pub(crate) fn semantic(
    snapshot: &Snapshot,
    mask: &Mask,
    vector: &[f32],
    kind: Option<&str>,
    k: usize,
    allowed_ids: Option<&[String]>,
) -> Result<Value> {
    if vector.len() != snapshot.dimensions {
        return Err("query vector dimensions differ from imported embeddings".into());
    }
    let mut vector = vector.to_vec();
    normalize(&mut vector)?;
    let allowed = allowed_ids.map(|ids| ids.iter().map(String::as_str).collect::<HashSet<_>>());
    let mut scores = Vec::new();
    for row in snapshot.embeddings.iter() {
        if !mask.visible(&row.meta.graph) || kind.is_some_and(|k| k != row.meta.kind) {
            continue;
        }
        if allowed
            .as_ref()
            .is_some_and(|ids| !ids.contains(row.meta.id.as_str()))
        {
            continue;
        }
        let score = row
            .vector
            .iter()
            .zip(&vector)
            .map(|(a, b)| a * b)
            .sum::<f32>();
        scores.push((row, score));
    }
    scores.sort_by(|(a, sa), (b, sb)| {
        sb.total_cmp(sa)
            .then(a.meta.id.cmp(&b.meta.id))
            .then(a.meta.kind.cmp(&b.meta.kind))
    });
    let results: Vec<Value> = scores
        .into_iter()
        .take(k.clamp(1, 100))
        .map(|(row, similarity)| {
            json!({"id": row.meta.id, "graph": row.meta.graph, "kind": row.meta.kind,
               "text": row.meta.text, "similarity": similarity})
        })
        .collect();
    Ok(json!({"results": results, "snapshot": snapshot.generation}))
}

pub(crate) fn traverse(
    snapshot: &Snapshot,
    mask: &Mask,
    start: &str,
    predicate: Option<&str>,
    depth: usize,
    limit: usize,
) -> Result<Value> {
    let start = NamedNode::new(start.to_owned()).map_err(|e| e.to_string())?;
    let predicate = predicate
        .map(|p| NamedNode::new(p.to_owned()))
        .transpose()
        .map_err(|e| e.to_string())?;
    let mut pending = VecDeque::from([(NamedOrBlankNode::NamedNode(start), 0)]);
    let mut expanded = HashSet::new();
    let mut edges = Vec::new();
    let mut truncated = false;
    let depth = depth.clamp(1, 6);
    let limit = limit.clamp(1, 10_000);
    'walk: while let Some((subject, level)) = pending.pop_front() {
        if level >= depth || !expanded.insert(subject.clone()) {
            continue;
        }
        let mut quads = snapshot
            .store
            .quads_for_pattern(
                Some(subject.as_ref()),
                predicate.as_ref().map(NamedNode::as_ref),
                None,
                None,
            )
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        quads.sort_by_cached_key(|q| q.to_string());
        for quad in quads {
            let GraphName::NamedNode(graph) = &quad.graph_name else {
                continue;
            };
            if !mask.visible(graph.as_str()) {
                continue;
            }
            if edges.len() == limit {
                truncated = true;
                break 'walk;
            }
            match &quad.object {
                Term::NamedNode(node) => {
                    pending.push_back((NamedOrBlankNode::NamedNode(node.clone()), level + 1))
                }
                Term::BlankNode(node) => {
                    pending.push_back((NamedOrBlankNode::BlankNode(node.clone()), level + 1))
                }
                _ => {}
            }
            edges.push(
                json!({"subject": quad.subject.to_string(), "predicate": quad.predicate.as_str(),
                              "object": quad.object.to_string(), "graph": graph.as_str()}),
            );
        }
    }
    Ok(json!({"edges": edges, "truncated": truncated, "snapshot": snapshot.generation}))
}

pub(crate) fn render(template: &str, parameters: &BTreeMap<String, Value>) -> Result<String> {
    validate_parameter_positions(template)?;
    let mut remaining = template;
    let mut out = String::new();
    let mut used = HashSet::new();
    while let Some(at) = remaining.find("{{") {
        out.push_str(&remaining[..at]);
        remaining = &remaining[at + 2..];
        let end = remaining
            .find("}}")
            .ok_or("unterminated saved-query parameter")?;
        let name = &remaining[..end];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err("saved-query parameter names use ASCII letters/digits/underscore".into());
        }
        let param = parameters
            .get(name)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("missing RDF-term parameter {name}"))?;
        if param.len() != 1 {
            return Err("parameters require exactly one iri or literal key".into());
        }
        let term = if let Some(iri) = param.get("iri").and_then(Value::as_str) {
            NamedNode::new(iri.to_owned())
                .map_err(|e| e.to_string())?
                .to_string()
        } else if let Some(literal) = param.get("literal").and_then(Value::as_str) {
            Literal::new_simple_literal(literal).to_string()
        } else {
            return Err("parameters require a string iri or literal".into());
        };
        out.push_str(&term);
        used.insert(name.to_owned());
        remaining = &remaining[end + 2..];
    }
    if remaining.contains("}}") {
        return Err("unmatched saved-query parameter terminator".into());
    }
    out.push_str(remaining);
    if used.len() != parameters.len() {
        return Err("unused saved-query parameters".into());
    }
    Ok(out)
}

// RDF-term escaping protects term positions. Putting a placeholder inside a
// quoted literal, an IRI, or a comment changes its lexical context and defeats
// that guarantee, so refuse those templates before substituting any values.
fn validate_parameter_positions(template: &str) -> Result<()> {
    #[derive(Clone, Copy)]
    enum Context {
        Term,
        String(u8, bool, bool),
        Iri,
        Comment,
    }
    let bytes = template.as_bytes();
    let mut context = Context::Term;
    let mut at = 0;
    while at < bytes.len() {
        let placeholder = bytes[at..].starts_with(b"{{");
        if placeholder && !matches!(context, Context::Term) {
            return Err(
                "saved-query placeholders must be RDF terms, outside strings, IRIs and comments"
                    .into(),
            );
        }
        match context {
            Context::Term if placeholder => {
                let end = template[at + 2..]
                    .find("}}")
                    .ok_or("unterminated saved-query parameter")?;
                at += end + 4;
                continue;
            }
            Context::Term => match bytes[at] {
                b'\'' | b'"' => {
                    let quote = bytes[at];
                    let long = bytes[at..].starts_with(&[quote, quote, quote]);
                    context = Context::String(quote, long, false);
                    at += if long { 3 } else { 1 };
                    continue;
                }
                b'#' => context = Context::Comment,
                b'<' => {
                    // Distinguish an IRI token from '<'/'<=' comparisons. Any
                    // whitespace-free <...> is treated as IRI context here;
                    // the SPARQL parser checks its full lexical validity.
                    if let Some(end) = bytes[at + 1..].iter().position(|b| *b == b'>') {
                        let body = &bytes[at + 1..at + 1 + end];
                        if !body.is_empty()
                            && !body[0].is_ascii_whitespace()
                            && body[0] != b'='
                            && !body.iter().any(u8::is_ascii_whitespace)
                        {
                            context = Context::Iri;
                        }
                    }
                }
                _ => {}
            },
            Context::String(quote, long, escaped) => {
                if escaped {
                    context = Context::String(quote, long, false);
                } else if bytes[at] == b'\\' {
                    context = Context::String(quote, long, true);
                } else if bytes[at] == quote {
                    if long && bytes[at..].starts_with(&[quote, quote, quote]) {
                        context = Context::Term;
                        at += 3;
                        continue;
                    } else if !long {
                        context = Context::Term;
                    }
                }
            }
            Context::Iri if bytes[at] == b'>' => context = Context::Term,
            Context::Comment if matches!(bytes[at], b'\n' | b'\r') => context = Context::Term,
            _ => {}
        }
        at += 1;
    }
    Ok(())
}

pub(crate) fn validate_template(template: &str) -> Result<()> {
    let mut remaining = template;
    let mut params = BTreeMap::new();
    while let Some(at) = remaining.find("{{") {
        remaining = &remaining[at + 2..];
        let end = remaining
            .find("}}")
            .ok_or("unterminated saved-query parameter")?;
        params.insert(
            remaining[..end].to_owned(),
            json!({"iri": "urn:swarm:placeholder"}),
        );
        remaining = &remaining[end + 2..];
    }
    let rendered = render(template, &params)?;
    if rendered.to_ascii_uppercase().contains("SERVICE") {
        return Err("swarm SPARQL SERVICE is disabled".into());
    }
    SparqlEvaluator::new()
        .parse_query(&rendered)
        .map_err(|e| e.to_string())?;
    Ok(())
}
