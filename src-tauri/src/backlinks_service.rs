//! Backlinks: who points at a document, a block or an object (SPRINT D10).
//!
//! One SPARQL `UNION` over the workspace, document, whiteboard and wire lanes of the graph's store, sorted and
//! deduplicated. A row says what kind of pointer it is, which document it comes from (and the block), that document's
//! title and kind, and, for wires, the predicate. It serves documents, objects (by IRI) and boards.
//!
//! The MCP tool `get_backlinks` and the REST route (`loopback_backlinks_routes`) are two doors onto
//! [`get_backlinks`]. Owned by G4. The signatures and types below are FINAL; G4 replaces the stub bodies.
//! U0 registers NO tool: G4 adds the registration sites listed in INTERFACES.md.

use crate::app_runtime::AppHandle;
use crate::rdf::{document_subject, sparql_string_literal};
use crate::whiteboard::links;
use oxigraph::model::{NamedNode, Term};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use std::collections::BTreeMap;

/// What kind of pointer a row is. Serialized (camelCase `kind`) as the lowercase-kebab strings in [`BacklinkKind::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BacklinkKind {
    /// An incoming wire: `wire:targetDocument` / `wire:targetBlock` / `wire:targetResource` is the target. Only
    /// incoming wires are reported (a bidirectional wire's `-inv` twin is skipped).
    Wire,
    /// A document block embeds the object (`mdoc:embeds <iri>`).
    Embed,
    /// A wikilink (`mdoc:targetDocId`) in a document block names the document.
    Wikilink,
    /// A document block's `mnemosyne://` link href names the document or object.
    Link,
    /// A board shape's `wb:refersTo` names the target.
    BoardLink,
    /// A board card's `wb:refersTo` names the target.
    BoardCard,
    /// A board connection ends at a shape that refers to the target; the row's `via_connection` names it. Reserved:
    /// G4 may fold this into `BoardLink` if the query cannot tell them apart, and says so in its commit.
    BoardConnection,
}

impl BacklinkKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BacklinkKind::Wire => "wire",
            BacklinkKind::Embed => "embed",
            BacklinkKind::Wikilink => "wikilink",
            BacklinkKind::Link => "link",
            BacklinkKind::BoardLink => "board-link",
            BacklinkKind::BoardCard => "board-card",
            BacklinkKind::BoardConnection => "board-connection",
        }
    }
}

/// What is being looked up. Exactly one of `document_id` and `iri`; `block_id` only with `document_id`.
/// A board is a document: look it up by its document id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BacklinkTarget {
    pub(crate) document_id: Option<String>,
    pub(crate) iri: Option<String>,
    pub(crate) block_id: Option<String>,
}

/// One incoming pointer. `source_document_id` is the document that holds the pointer (the board, for board kinds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Backlink {
    pub(crate) kind: BacklinkKind,
    pub(crate) source_document_id: String,
    /// The block (or, on a board, the shape element id) the pointer sits in.
    pub(crate) source_block_id: Option<String>,
    pub(crate) source_title: Option<String>,
    /// The source document's `documentKind` (`whiteboard`, `flow-board`, ...), when it has one.
    pub(crate) source_kind: Option<String>,
    /// For `Wire`: the short predicate name (`supports`) and the wire id.
    pub(crate) predicate: Option<String>,
    pub(crate) wire_id: Option<String>,
    /// For `BoardConnection`: the arrow's element id on the board.
    pub(crate) via_connection: Option<String>,
}

/// The sort key is `(kind, source_document_id, source_block_id, wire_id, via_connection)`: stable across calls.
/// Identical rows are reported once.
pub(crate) fn get_backlinks(
    store: &Store,
    graph_id: &str,
    target: &BacklinkTarget,
) -> Result<Vec<Backlink>, String> {
    let Some(resolved) = resolve_target(graph_id, target)? else {
        // Nothing can point at what is not an IRI: an empty list, not an error.
        return Ok(Vec::new());
    };
    let query = incoming_query(graph_id, &resolved);
    let mut rows = Vec::new();
    for solution in solutions(store, &query)? {
        if let Some(row) = row_from_solution(&solution) {
            rows.push(row);
        }
    }

    // A document does not back-link to itself: a self-wire, or a board's own shape that refers to the board,
    // says nothing a reader did not already know.
    if let Resolved::Document {
        document_id,
        block_id,
        ..
    } = &resolved
    {
        rows.retain(|row| {
            !(row.source_document_id == *document_id
                && (block_id.is_none() || row.source_block_id == *block_id))
        });
    }

    // Titles and kinds are the workspace lane's, by source document.
    let documents = document_titles(store)?;
    for row in &mut rows {
        if let Some((title, kind)) = documents.get(&row.source_document_id) {
            row.source_title = title.clone();
            row.source_kind = kind.clone();
        }
    }

    rows.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    rows.dedup();
    Ok(rows)
}

/// The key rows are ordered by: the contract's five fields, then the rest so identical rows end up adjacent.
fn sort_key(
    row: &Backlink,
) -> (
    BacklinkKind,
    &str,
    Option<&str>,
    Option<&str>,
    Option<&str>,
    Option<&str>,
    Option<&str>,
    Option<&str>,
) {
    (
        row.kind,
        row.source_document_id.as_str(),
        row.source_block_id.as_deref(),
        row.wire_id.as_deref(),
        row.via_connection.as_deref(),
        row.predicate.as_deref(),
        row.source_title.as_deref(),
        row.source_kind.as_deref(),
    )
}

/// What the lookup is, with the IRIs the query will name. `None` from [`resolve_target`] means the target cannot be
/// written as an IRI, so nothing in the store can point at it.
enum Resolved {
    Document {
        document_id: String,
        block_id: Option<String>,
        /// `urn:mnemosyne:local:document:{id}`.
        subject: String,
        /// `{subject}#block-{block}` when a block was asked.
        block_ref: Option<String>,
    },
    Object {
        iri: String,
    },
}

fn resolve_target(graph_id: &str, target: &BacklinkTarget) -> Result<Option<Resolved>, String> {
    let nonempty = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let document_id = nonempty(&target.document_id);
    let iri = nonempty(&target.iri);
    let block_id = nonempty(&target.block_id);
    match (document_id, iri) {
        (Some(_), Some(_)) | (None, None) => {
            Err("get_backlinks: give exactly one of documentId and iri".to_string())
        }
        (Some(document_id), None) => {
            let subject = document_subject(&document_id);
            let block_ref = block_id
                .as_deref()
                .map(|block| crate::rdf_workspace_terms::block_ref_uri(&document_id, block));
            let _ = graph_id;
            let writable = NamedNode::new(subject.as_str()).is_ok()
                && block_ref
                    .as_deref()
                    .map_or(true, |block| NamedNode::new(block).is_ok());
            Ok(writable.then_some(Resolved::Document {
                document_id,
                block_id,
                subject,
                block_ref,
            }))
        }
        (None, Some(iri)) => {
            if block_id.is_some() {
                return Err("get_backlinks: blockId goes with documentId, not iri".to_string());
            }
            Ok(NamedNode::new(iri.as_str())
                .is_ok()
                .then_some(Resolved::Object { iri }))
        }
    }
}

const PREFIXES: &str = "PREFIX doc: <http://mnemosyne.dev/doc#> \
PREFIX mnemo: <https://mnemosyne.local/ns#> \
PREFIX wire: <http://mnemosyne.ai/vocab#> \
PREFIX wb: <http://mnemosyne.dev/whiteboard#> \
PREFIX dcterms: <http://purl.org/dc/terms/> ";

/// The one `UNION` over the lanes. Every pattern is inside `GRAPH ?g { ... }`: the cell's store is partitioned
/// into named graphs, and an unwrapped pattern matches nothing there. A graph that holds no whiteboard lane
/// simply matches nothing in those branches.
fn incoming_query(graph_id: &str, target: &Resolved) -> String {
    let mut branches: Vec<String> = Vec::new();

    // The source of a pointer that sits in a document's body: the node's document and its block (the node's own
    // `doc:nodeId`, else its parent's, as for an inline wikilink).
    let in_a_document = "?n mnemo:documentId ?nd . \
        OPTIONAL { ?n doc:nodeId ?nid } \
        OPTIONAL { ?pn doc:childNode ?n . ?pn doc:nodeId ?pnid }";
    let wire_tail = "OPTIONAL { ?w wire:sourceDocument ?sdoc } \
        OPTIONAL { ?w wire:sceneDocumentId ?scd } \
        OPTIONAL { ?w wire:sourceBlock ?sblk } \
        OPTIONAL { ?w wire:sceneSourceElementId ?sse } \
        OPTIONAL { ?w wire:predicate ?pred } \
        FILTER NOT EXISTS { ?w wire:deletedAt ?deleted } \
        FILTER(!STRENDS(STR(?w), \"-inv\"))";

    let (refers_to, wire_target) = match target {
        Resolved::Document {
            subject, block_ref, ..
        } => (
            subject.as_str(),
            match block_ref {
                Some(block) => {
                    format!("?w wire:targetDocument <{subject}> . ?w wire:targetBlock <{block}> .")
                }
                None => format!("?w wire:targetDocument <{subject}> ."),
            },
        ),
        Resolved::Object { iri } => (iri.as_str(), format!("?w wire:targetResource <{iri}> .")),
    };

    // Wire: an incoming wire (a bidirectional wire's `-inv` twin and a deleted wire are skipped).
    branches.push(format!(
        "{{ GRAPH ?g {{ ?w a wire:Wire . {wire_target} {wire_tail} }} BIND(\"wire\" AS ?kind) }}"
    ));

    // The other kinds point at a whole document or object, not at a block of it.
    let whole = match target {
        Resolved::Document { block_id, .. } => block_id.is_none(),
        Resolved::Object { .. } => true,
    };
    if whole {
        match target {
            Resolved::Object { iri } => {
                // Embed: a document block embeds the object.
                branches.push(format!(
                    "{{ GRAPH ?g {{ ?n doc:embeds <{iri}> . {in_a_document} }} BIND(\"embed\" AS ?kind) }}"
                ));
            }
            Resolved::Document { document_id, .. } => {
                // Wikilink: names the document, in this graph (a wikilink with a target graph names another).
                let id = sparql_string_literal(document_id);
                let graph = sparql_string_literal(graph_id);
                branches.push(format!(
                    "{{ GRAPH ?g {{ ?n doc:targetDocId {id} . {in_a_document} \
                     OPTIONAL {{ ?n doc:targetGraphId ?tg }} \
                     FILTER(!BOUND(?tg) || ?tg = {graph}) }} BIND(\"wikilink\" AS ?kind) }}"
                ));
            }
        }
        // Link: a `mnemosyne://` href naming the target, encoded or decoded.
        let hrefs = link_hrefs(graph_id, target)
            .iter()
            .map(|href| sparql_string_literal(href))
            .collect::<Vec<_>>()
            .join(", ");
        branches.push(format!(
            "{{ GRAPH ?g {{ ?n doc:href ?href . FILTER(?href IN ({hrefs})) {in_a_document} }} BIND(\"link\" AS ?kind) }}"
        ));
        // Board: a shape or card that refers to the target.
        branches.push(format!(
            "{{ GRAPH ?g {{ ?bs wb:refersTo <{refers_to}> . ?bs wb:documentId ?bdoc . ?bs a ?cls . \
             FILTER(?cls = wb:Shape || ?cls = wb:Card) }} \
             BIND(IF(?cls = wb:Card, \"board-card\", \"board-link\") AS ?kind) }}"
        ));
        // Board connection: an arrow that ends at a shape that refers to the target.
        branches.push(format!(
            "{{ GRAPH ?g {{ ?bs wb:refersTo <{refers_to}> . ?bc a wb:Connection . ?bc wb:documentId ?bdoc . \
             {{ ?bc wb:from ?bs }} UNION {{ ?bc wb:to ?bs }} }} BIND(\"board-connection\" AS ?kind) }}"
        ));
    }

    format!(
        "{PREFIXES}SELECT ?kind ?w ?sdoc ?scd ?sblk ?sse ?pred ?nd ?nid ?pnid ?bdoc ?bs ?bc WHERE {{ {} }}",
        branches.join(" UNION ")
    )
}

/// The hrefs a document body may carry for the target: the encoded link (what the app writes) and the decoded one.
fn link_hrefs(graph_id: &str, target: &Resolved) -> Vec<String> {
    let mut hrefs = match target {
        Resolved::Document { document_id, .. } => vec![
            links::document_link(graph_id, document_id),
            format!("mnemosyne://document/{graph_id}/{document_id}"),
        ],
        Resolved::Object { iri } => vec![
            links::object_link(graph_id, iri),
            format!("mnemosyne://object/{graph_id}/{iri}"),
        ],
    };
    hrefs.sort();
    hrefs.dedup();
    hrefs
}

fn solutions(store: &Store, query: &str) -> Result<Vec<oxigraph::sparql::QuerySolution>, String> {
    let results = SparqlEvaluator::new()
        .parse_query(query)
        .map_err(|error| format!("get_backlinks: query: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("get_backlinks: query: {error}"))?;
    let QueryResults::Solutions(solutions) = results else {
        return Err("get_backlinks: the query did not answer rows".to_string());
    };
    solutions
        .map(|solution| solution.map_err(|error| format!("get_backlinks: row: {error}")))
        .collect()
}

/// The text of a bound term: an IRI's, or a literal's value.
fn text(solution: &oxigraph::sparql::QuerySolution, name: &str) -> Option<String> {
    match solution.get(name)? {
        Term::NamedNode(node) => Some(node.as_str().to_string()),
        Term::Literal(literal) => Some(literal.value().to_string()),
        _ => None,
    }
}

/// What follows the last `marker` in `value`.
fn tail_after(value: &str, marker: &str) -> Option<String> {
    value
        .rsplit_once(marker)
        .map(|(_, tail)| tail.to_string())
        .filter(|tail| !tail.is_empty())
}

/// `urn:mnemosyne:local:document:{id}` (optionally with a fragment) to the document id.
fn document_id_of(subject: &str) -> String {
    let subject = subject.split('#').next().unwrap_or(subject);
    subject
        .strip_prefix(&document_subject(""))
        .unwrap_or(subject)
        .to_string()
}

fn blank_row(kind: BacklinkKind, source_document_id: String) -> Backlink {
    Backlink {
        kind,
        source_document_id,
        source_block_id: None,
        source_title: None,
        source_kind: None,
        predicate: None,
        wire_id: None,
        via_connection: None,
    }
}

/// One solution of the union as a row; `None` when it does not say which document it comes from.
fn row_from_solution(solution: &oxigraph::sparql::QuerySolution) -> Option<Backlink> {
    let kind = text(solution, "kind")?;
    match kind.as_str() {
        "wire" => {
            // From a document, else (an object source, drawn on a board) from the board.
            let source_document_id = text(solution, "sdoc")
                .map(|subject| document_id_of(&subject))
                .or_else(|| text(solution, "scd"))?;
            let mut row = blank_row(BacklinkKind::Wire, source_document_id);
            row.source_block_id = text(solution, "sblk")
                .and_then(|block| tail_after(&block, "#block-"))
                .or_else(|| text(solution, "sse"));
            row.predicate = text(solution, "pred")
                .map(|uri| crate::wire_predicates::predicate_short_name(&uri));
            row.wire_id = text(solution, "w")
                .map(|subject| tail_after(&subject, ":wire:").unwrap_or(subject));
            Some(row)
        }
        "embed" | "wikilink" | "link" => {
            let kind = match kind.as_str() {
                "embed" => BacklinkKind::Embed,
                "wikilink" => BacklinkKind::Wikilink,
                _ => BacklinkKind::Link,
            };
            let mut row = blank_row(kind, text(solution, "nd")?);
            row.source_block_id = text(solution, "nid").or_else(|| text(solution, "pnid"));
            Some(row)
        }
        "board-link" | "board-card" => {
            let kind = if kind == "board-card" {
                BacklinkKind::BoardCard
            } else {
                BacklinkKind::BoardLink
            };
            let mut row = blank_row(kind, text(solution, "bdoc")?);
            row.source_block_id =
                text(solution, "bs").and_then(|subject| tail_after(&subject, "#block-"));
            Some(row)
        }
        "board-connection" => {
            let mut row = blank_row(BacklinkKind::BoardConnection, text(solution, "bdoc")?);
            row.source_block_id =
                text(solution, "bs").and_then(|subject| tail_after(&subject, "#block-"));
            row.via_connection =
                text(solution, "bc").and_then(|subject| tail_after(&subject, "#arrow-"));
            Some(row)
        }
        _ => None,
    }
}

/// `(title, documentKind)` of every document the workspace lane knows, by document id.
fn document_titles(
    store: &Store,
) -> Result<BTreeMap<String, (Option<String>, Option<String>)>, String> {
    let query = format!(
        "{PREFIXES}SELECT ?d ?t ?k WHERE {{ GRAPH ?g {{ ?d a doc:TipTapDocument . \
         OPTIONAL {{ ?d dcterms:title ?t }} OPTIONAL {{ ?d doc:documentKind ?k }} }} }}"
    );
    let mut documents: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    for solution in solutions(store, &query)? {
        let Some(subject) = text(&solution, "d") else {
            continue;
        };
        let entry = documents.entry(document_id_of(&subject)).or_default();
        entry.0 = entry.0.take().or_else(|| text(&solution, "t"));
        entry.1 = entry.1.take().or_else(|| text(&solution, "k"));
    }
    Ok(documents)
}

/// The JSON of one row, the contract both doors return (camelCase): `{kind, sourceDocumentId, sourceBlockId?,
/// sourceTitle?, sourceKind?, predicate?, wireId?, viaConnection?}`; absent options are omitted, not null.
pub(crate) fn backlink_json(row: &Backlink) -> serde_json::Value {
    let mut value = serde_json::json!({
        "kind": row.kind.as_str(),
        "sourceDocumentId": row.source_document_id,
    });
    for (key, field) in [
        ("sourceBlockId", &row.source_block_id),
        ("sourceTitle", &row.source_title),
        ("sourceKind", &row.source_kind),
        ("predicate", &row.predicate),
        ("wireId", &row.wire_id),
        ("viaConnection", &row.via_connection),
    ] {
        if let Some(field) = field {
            value[key] = serde_json::json!(field);
        }
    }
    value
}

/// The MCP tool `get_backlinks {graphId, documentId? | iri?, blockId?}`. Returns
/// `{graphId, target: {documentId?, iri?, blockId?}, backlinks: [row...], count}`.
pub(crate) fn mcp_local_get_backlinks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use crate::mcp_utils::{mcp_arg_string, mcp_graph_id_or_default};
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let target = BacklinkTarget {
        document_id: mcp_arg_string(arguments, &["documentId", "document_id"]),
        iri: mcp_arg_string(arguments, &["iri"]),
        block_id: mcp_arg_string(arguments, &["blockId", "block_id"]),
    };
    // Asked before the graph is: a malformed call is an error whatever graph it names.
    if target.document_id.is_some() == target.iri.is_some() {
        return Err("get_backlinks: give exactly one of documentId and iri".to_string());
    }
    if target.iri.is_some() && target.block_id.is_some() {
        return Err("get_backlinks: blockId goes with documentId, not iri".to_string());
    }

    let rows = match crate::graph_paths::existing_graph_dir_no_heal(&app, &graph_id) {
        // An unknown graph has no backlinks.
        Err(_) => Vec::new(),
        Ok(graph_dir) => {
            let store = crate::rdf_store_service::open_graph_store(&graph_dir)?;
            get_backlinks(&store, &graph_id, &target)?
        }
    };

    let mut echoed = serde_json::Map::new();
    for (key, field) in [
        ("documentId", &target.document_id),
        ("iri", &target.iri),
        ("blockId", &target.block_id),
    ] {
        if let Some(field) = field {
            echoed.insert(key.to_string(), serde_json::json!(field));
        }
    }
    Ok(serde_json::json!({
        "graphId": graph_id,
        "target": echoed,
        "backlinks": rows.iter().map(backlink_json).collect::<Vec<_>>(),
        "count": rows.len(),
    }))
}
