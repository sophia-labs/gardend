//! Files in search (2026-10-14): `search_documents` and `search_blocks` also
//! find the graph's files, and a `datasheet_facts` miss says where the part is
//! named instead.
//!
//! Why. A datasheet PDF is a file, not a document, so the two search tools never
//! saw one, and a chat model whose first search came back empty stopped there
//! ("Which buck regulators are in the library?" over a library of 28 PDFs and no
//! documents; V-followups-vehicle, 2026-10-13).
//!
//! What matches. A live file (an artifact in the workspace) by its name and,
//! when `datasheet_import` read it, by its datasheet's title, part numbers,
//! maker, product type, topology, category and document type. Each hit is typed
//! (`kind`: `datasheet` or `file`) and carries the `artifactId` that
//! `datasheet_facts` (`artifact`) and `read_artifact` (`artifact_id`) take.
//!
//! Shape. Hits ride beside a search's `results` as `files` (plus `fileTotal`),
//! never inside them: `results` keeps its exact shape for every existing caller
//! (hosted Choreograph agents, and the Shrubbery search routes, which map
//! `results` only and opt out of the file pass with `includeFiles: false`).
//!
//! Cost. Only a graph that holds files is looked at: its workspace snapshot,
//! and its RDF store for the datasheet editions (one `rdf:type` index lookup,
//! then each edition's own few quads). A graph without files never opens its
//! store from a search.

use std::collections::{BTreeMap, BTreeSet};

use oxigraph::model::{GraphNameRef, NamedNode, NamedNodeRef, NamedOrBlankNode, Term};
use oxigraph::store::Store;
use serde_json::{json, Value};

use crate::datasheet_import::{de, eng, norm_ident, RDF_TYPE};
use crate::document_record_store::read_graph_documents_cold;
use crate::hosted_navigation_projection::hosted_artifact_value;
use crate::rdf_service::snapshot_array;
use crate::search_lexical_projection::ranked_lexical_block_hits;
use crate::workspace_entity_projection::{workspace_entity_id, workspace_entity_title};

/// At most this many file hits ride with one search.
pub(crate) const MAX_FILE_HITS: usize = 10;
/// A `datasheet_facts` miss names at most this many documents and files.
const MAX_MENTION_DOCUMENTS: usize = 5;
const MAX_MENTION_BLOCKS_PER_DOCUMENT: usize = 2;
const MENTION_SNIPPET_CHARS: usize = 160;

/// Words that carry no search meaning on their own ("which buck regulators are
/// in the library" is about "buck regulators").
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "any", "are", "as", "at", "by", "for", "from", "has", "have", "in", "is",
    "it", "its", "of", "on", "or", "our", "that", "the", "their", "there", "these", "this",
    "those", "to", "what", "which", "who", "with",
];

/// One searchable file: a live workspace artifact, with its datasheet edition
/// when `datasheet_import` read it (or an edition imported inline, with no file).
#[derive(Debug, Clone, Default)]
pub(crate) struct FileEntry {
    pub(crate) artifact_id: Option<String>,
    pub(crate) name: String,
    pub(crate) original_name: Option<String>,
    pub(crate) folder_id: Option<String>,
    pub(crate) folder: Option<String>,
    pub(crate) mime_type: Option<String>,
    pub(crate) file_type: Option<String>,
    pub(crate) size_bytes: Option<f64>,
    pub(crate) datasheet: Option<DatasheetCard>,
}

/// The few fields of a `de:DocumentEdition` that say what a datasheet is.
#[derive(Debug, Clone, Default)]
pub(crate) struct DatasheetCard {
    pub(crate) title: Option<String>,
    pub(crate) parts: Vec<String>,
    pub(crate) maker: Option<String>,
    pub(crate) product_type: Option<String>,
    pub(crate) topology: Option<String>,
    pub(crate) category: Option<String>,
    pub(crate) document_type: Option<String>,
    pub(crate) file_name: Option<String>,
    pub(crate) aliases: Vec<String>,
    pub(crate) artifact_id: Option<String>,
    pub(crate) text_artifact_id: Option<String>,
    /// Artifacts the import recorded as its renditions (text, regions, the
    /// envelope): folded into this entry, never listed on their own.
    pub(crate) rendition_artifacts: Vec<String>,
}

impl FileEntry {
    fn kind(&self) -> &'static str {
        if self.datasheet.is_some() {
            "datasheet"
        } else {
            "file"
        }
    }

    fn title(&self) -> String {
        self.datasheet
            .as_ref()
            .and_then(|d| d.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| self.name.clone())
    }

    fn parts(&self) -> &[String] {
        self.datasheet
            .as_ref()
            .map(|d| d.parts.as_slice())
            .unwrap_or(&[])
    }

    /// The file's names: its own, the uploaded one, and the ones its datasheet
    /// import recorded.
    fn file_names(&self) -> Vec<String> {
        let mut names = vec![self.name.clone()];
        names.extend(self.original_name.clone());
        if let Some(d) = &self.datasheet {
            names.extend(d.file_name.clone());
            names.extend(d.aliases.iter().cloned());
        }
        names.retain(|n| !n.trim().is_empty());
        names
    }

    /// Every word that says what the file is.
    fn words(&self) -> String {
        let mut text = self.file_names();
        if let Some(d) = &self.datasheet {
            text.extend(d.title.clone());
            text.extend(d.parts.iter().cloned());
            for v in [
                &d.maker,
                &d.product_type,
                &d.topology,
                &d.category,
                &d.document_type,
            ] {
                text.extend(v.clone());
            }
        }
        text.extend(self.folder.clone());
        text.join(" ")
    }
}

// ------------------------------------------------------------------- loading

/// The graph's live files, each with its datasheet edition if it has one, plus
/// the datasheets imported with no file. Empty for a graph with no files: its
/// store is not opened.
pub(crate) fn load_file_entries(
    graph_id: &str,
    graph_dir: &std::path::Path,
) -> Result<Vec<FileEntry>, String> {
    let record = crate::document_service::read_workspace_record(graph_dir, graph_id)?;
    let Some(snapshot) = record.snapshot.as_ref() else {
        return Ok(Vec::new());
    };
    let artifacts = snapshot_array(snapshot, "artifacts");
    if artifacts.is_empty() {
        return Ok(Vec::new());
    }
    let folders: BTreeMap<String, String> = snapshot_array(snapshot, "folders")
        .iter()
        .filter_map(|folder| {
            Some((
                workspace_entity_id(folder)?,
                workspace_entity_title(folder)?,
            ))
        })
        .collect();
    let cards = if graph_dir.join("store.oxigraph").is_dir() {
        let store = crate::rdf_service::open_graph_store(graph_dir)?;
        datasheet_cards(&store, graph_id)
    } else {
        Vec::new()
    };
    Ok(entries_from(artifacts, &folders, cards, graph_id))
}

fn entries_from(
    artifacts: &[Value],
    folders: &BTreeMap<String, String>,
    cards: Vec<DatasheetCard>,
    graph_id: &str,
) -> Vec<FileEntry> {
    let mut live: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for entity in artifacts {
        let value = hosted_artifact_value(entity, graph_id);
        let Some(id) = value["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        let name = value["label"].as_str().unwrap_or(&id).to_string();
        let original = value["originalFilename"]
            .as_str()
            .filter(|o| *o != name)
            .map(str::to_string);
        let folder_id = value["parentId"].as_str().map(str::to_string);
        let folder = folder_id.as_ref().and_then(|f| folders.get(f).cloned());
        order.push(id.clone());
        live.insert(
            id.clone(),
            FileEntry {
                artifact_id: Some(id),
                name,
                original_name: original,
                folder_id,
                folder,
                mime_type: value["mimeType"].as_str().map(str::to_string),
                file_type: value["fileType"].as_str().map(str::to_string),
                size_bytes: value["sizeBytes"].as_f64(),
                datasheet: None,
            },
        );
    }
    let renditions: BTreeSet<String> = cards
        .iter()
        .flat_map(|card| card.rendition_artifacts.iter().cloned())
        .collect();
    let mut unbound: Vec<FileEntry> = Vec::new();
    for card in cards {
        match card.artifact_id.clone() {
            Some(artifact) => {
                // A datasheet whose file is gone (trashed, purged) is not offered by search.
                if let Some(entry) = live.get_mut(&artifact) {
                    entry.datasheet = Some(card);
                }
            }
            None => unbound.push(FileEntry {
                name: card
                    .file_name
                    .clone()
                    .or_else(|| card.title.clone())
                    .unwrap_or_else(|| card.parts.join(", ")),
                datasheet: Some(card),
                ..FileEntry::default()
            }),
        }
    }
    let mut entries: Vec<FileEntry> = Vec::new();
    for id in order {
        let Some(entry) = live.remove(&id) else {
            continue;
        };
        // A rendition (the text a read job wrote, its envelope) rides with its datasheet.
        if renditions.contains(&id) && entry.datasheet.is_none() {
            continue;
        }
        entries.push(entry);
    }
    entries.extend(unbound);
    entries
}

fn literal_text(term: &Term) -> Option<String> {
    match term {
        Term::Literal(literal) => Some(literal.value().to_string()),
        _ => None,
    }
}

fn named(term: &Term) -> Option<NamedNode> {
    match term {
        Term::NamedNode(node) => Some(node.clone()),
        _ => None,
    }
}

/// Every `de:DocumentEdition` this graph's datasheet imports hold, in import
/// graph order: one `rdf:type` index lookup, then each edition's own quads.
pub(crate) fn datasheet_cards(store: &Store, graph_id: &str) -> Vec<DatasheetCard> {
    let prefix = format!(
        "{}:projection:datasheet:ds-",
        crate::rdf::graph_subject(graph_id)
    );
    let rdf_type = NamedNode::new_unchecked(RDF_TYPE);
    let edition_class = NamedNode::new_unchecked(de("DocumentEdition"));
    let mut editions: Vec<(NamedNode, NamedNode)> = store
        .quads_for_pattern(
            None,
            Some(rdf_type.as_ref()),
            Some(edition_class.as_ref().into()),
            None,
        )
        .filter_map(Result::ok)
        .filter_map(|quad| {
            let oxigraph::model::GraphName::NamedNode(graph) = quad.graph_name else {
                return None;
            };
            if !graph.as_str().starts_with(&prefix) {
                return None;
            }
            let NamedOrBlankNode::NamedNode(subject) = quad.subject else {
                return None;
            };
            Some((graph, subject))
        })
        .collect();
    editions.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    editions
        .iter()
        .map(|(graph, edition)| edition_card(store, graph.as_ref(), edition.as_ref()))
        .collect()
}

fn props_of(
    store: &Store,
    graph: NamedNodeRef<'_>,
    subject: NamedNodeRef<'_>,
) -> BTreeMap<String, Vec<Term>> {
    let mut props: BTreeMap<String, Vec<Term>> = BTreeMap::new();
    for quad in store
        .quads_for_pattern(
            Some(subject.into()),
            None,
            None,
            Some(GraphNameRef::NamedNode(graph)),
        )
        .filter_map(Result::ok)
    {
        props
            .entry(quad.predicate.into_string())
            .or_default()
            .push(quad.object);
    }
    props
}

fn first_text(props: &BTreeMap<String, Vec<Term>>, predicate: &str) -> Option<String> {
    props
        .get(predicate)?
        .iter()
        .find_map(literal_text)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn edition_card(
    store: &Store,
    graph: NamedNodeRef<'_>,
    edition: NamedNodeRef<'_>,
) -> DatasheetCard {
    let props = props_of(store, graph, edition);
    let mut parts: Vec<String> = Vec::new();
    for family in props
        .get(&de("namesFamily"))
        .into_iter()
        .flatten()
        .filter_map(named)
    {
        let family_props = props_of(store, graph, family.as_ref());
        if let Some(part) = first_text(&family_props, &eng("identifier"))
            .or_else(|| first_text(&family_props, &eng("normalizedIdentifier")))
        {
            if !parts.iter().any(|p| norm_ident(p) == norm_ident(&part)) {
                parts.push(part);
            }
        }
    }
    if let Some(part) = first_text(&props, &de("partNumber")) {
        if !parts.iter().any(|p| norm_ident(p) == norm_ident(&part)) {
            parts.push(part);
        }
    }
    // The file the import read, and its renditions (text, envelope, regions).
    let rdf_type = NamedNode::new_unchecked(RDF_TYPE);
    let binding_class = NamedNode::new_unchecked(de("FileSourceBinding"));
    let rendition_class = NamedNode::new_unchecked(de("Rendition"));
    let mut bound_artifact = None;
    let mut text_artifact = None;
    let mut rendition_artifacts = Vec::new();
    for (class, is_binding) in [(&binding_class, true), (&rendition_class, false)] {
        for quad in store
            .quads_for_pattern(
                None,
                Some(rdf_type.as_ref()),
                Some(class.as_ref().into()),
                Some(GraphNameRef::NamedNode(graph)),
            )
            .filter_map(Result::ok)
        {
            let NamedOrBlankNode::NamedNode(node) = quad.subject else {
                continue;
            };
            let node_props = props_of(store, graph, node.as_ref());
            let Some(artifact) = first_text(&node_props, &de("artifact")) else {
                continue;
            };
            if is_binding {
                bound_artifact.get_or_insert(artifact);
            } else {
                let role = node_props
                    .get(&de("renditionRole"))
                    .into_iter()
                    .flatten()
                    .filter_map(named)
                    .map(|r| {
                        r.as_str()
                            .rsplit(['#', '/'])
                            .next()
                            .unwrap_or("")
                            .to_string()
                    })
                    .next();
                if role.as_deref() == Some("text") {
                    text_artifact.get_or_insert(artifact.clone());
                }
                rendition_artifacts.push(artifact);
            }
        }
    }
    DatasheetCard {
        title: first_text(&props, &de("title")),
        parts,
        maker: first_text(&props, &de("maker")),
        product_type: first_text(&props, &de("productType")).filter(|v| v != "unknown"),
        topology: first_text(&props, &de("topology")),
        category: first_text(&props, &de("category")),
        document_type: first_text(&props, &de("documentType")),
        file_name: first_text(&props, &de("fileName")),
        aliases: props
            .get(&de("alias"))
            .into_iter()
            .flatten()
            .filter_map(literal_text)
            .collect(),
        artifact_id: first_text(&props, &de("artifact")).or(bound_artifact),
        text_artifact_id: text_artifact,
        rendition_artifacts,
    }
}

// ------------------------------------------------------------------ matching

fn search_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in text
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .map(|w| w.trim_matches(|c: char| c == '-' || c == '_' || c == '.'))
        .filter(|w| !w.is_empty())
    {
        words.push(word.to_string());
        let parts: Vec<&str> = word
            .split(['-', '_', '.'])
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() > 1 {
            words.extend(parts.iter().map(|p| p.to_string()));
            words.push(parts.concat());
        }
    }
    words.sort();
    words.dedup();
    words
}

fn query_words(query: &str) -> Vec<String> {
    let mut words: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .map(|w| w.trim_matches(|c: char| c == '-' || c == '_' || c == '.'))
        .filter(|w| w.chars().count() >= 2 && !STOP_WORDS.contains(w))
        .map(str::to_string)
        .collect();
    words.dedup();
    words
}

/// A plural's singular, for word matching only ("regulators" ~ "regulator").
fn singular(word: &str) -> &str {
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        &word[..word.len() - 1]
    } else {
        word
    }
}

/// The share of the query's words the file's words hold: 1 for an exact word,
/// 1/2 when one holds the other (both at least 3 characters long).
fn word_overlap(query: &[String], text: &[String]) -> f64 {
    if query.is_empty() || text.is_empty() {
        return 0.0;
    }
    let mut score = 0.0;
    for q in query {
        let q = singular(q);
        if text.iter().any(|t| singular(t) == q) {
            score += 1.0;
        } else if q.len() >= 3
            && text
                .iter()
                .any(|t| t.len() >= 3 && (t.contains(q) || q.contains(t.as_str())))
        {
            score += 0.5;
        }
    }
    score / query.len() as f64
}

/// True when the text holds at least one of the query's words whole: a match
/// made of fragments only ("converts" in "converter") is not one.
fn has_whole_word(query: &[String], text: &[String]) -> bool {
    query
        .iter()
        .any(|q| text.iter().any(|t| singular(t) == singular(q)))
}

/// The words of every file in the graph: a query word none of them holds
/// ("library" in "which buck regulators are in the library?") says nothing
/// about which file is meant, so it does not count against a match.
pub(crate) struct FileVocabulary(Vec<String>);

impl FileVocabulary {
    pub(crate) fn of(entries: &[FileEntry]) -> Self {
        let mut words: Vec<String> = entries
            .iter()
            .flat_map(|entry| search_words(&entry.words()))
            .collect();
        words.sort();
        words.dedup();
        Self(words)
    }

    fn knows(&self, word: &str) -> bool {
        word_overlap(&[word.to_string()], &self.0) > 0.0
    }
}

/// How well one query names a file, and on what: `exact`, `part`, `title`,
/// `filename` or `words`. Mirrors the document ladder (exact > prefix >
/// substring > words); `exact` mode stops after the exact tier, and only the
/// ranked modes (`auto` and `search_blocks`' own) reach the words tier.
pub(crate) fn file_match(
    entry: &FileEntry,
    query: &str,
    mode: &str,
    vocabulary: &FileVocabulary,
) -> Option<(f64, &'static str)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    let qn = norm_ident(&q);
    let file_names: Vec<String> = entry
        .file_names()
        .iter()
        .map(|n| n.to_lowercase())
        .collect();
    let title = entry
        .datasheet
        .as_ref()
        .and_then(|d| d.title.as_ref())
        .map(|t| t.to_lowercase());
    let stem = |n: &str| {
        n.rsplit_once('.')
            .map_or(n.to_string(), |(s, _)| s.to_string())
    };
    if file_names.iter().any(|n| *n == q || stem(n) == q) || title.as_deref() == Some(q.as_str()) {
        return Some((1.0, "exact"));
    }
    let parts: Vec<String> = entry.parts().iter().map(|p| norm_ident(p)).collect();
    if !qn.is_empty() && parts.iter().any(|p| *p == qn) {
        return Some((1.0, "part"));
    }
    if mode == "exact" {
        return None;
    }
    if qn.len() >= 3
        && parts
            .iter()
            .any(|p| p.starts_with(&qn) || (qn.len() >= 4 && p.contains(&qn)))
    {
        return Some((0.9, "part"));
    }
    if let Some(title) = &title {
        if title.starts_with(&q) {
            return Some((0.9, "title"));
        }
        if title.contains(&q) {
            return Some((0.75, "title"));
        }
    }
    if file_names.iter().any(|n| n.starts_with(&q)) {
        return Some((0.85, "filename"));
    }
    if file_names.iter().any(|n| n.contains(&q)) {
        return Some((0.7, "filename"));
    }
    // "LM5164" in "ti-lm5164_DS.pdf"; "tps 54331" in "TPS54331".
    if qn.len() >= 4 && file_names.iter().any(|n| norm_ident(n).contains(&qn)) {
        return Some((0.7, "filename"));
    }
    if !matches!(mode, "auto" | "hybrid" | "lexical" | "semantic") {
        return None;
    }
    let words = query_words(&q);
    // A part number among the query's words ("LM5164 max input voltage").
    if words
        .iter()
        .map(|w| norm_ident(w))
        .any(|w| w.len() >= 3 && parts.iter().any(|p| *p == w))
    {
        return Some((0.95, "part"));
    }
    let telling: Vec<String> = words.into_iter().filter(|w| vocabulary.knows(w)).collect();
    let text = search_words(&entry.words());
    let overlap = word_overlap(&telling, &text);
    if overlap >= 0.5 && has_whole_word(&telling, &text) {
        return Some((0.6 * overlap, "words"));
    }
    None
}

fn file_hit_json(entry: &FileEntry, score: f64, query: &str, matched_on: &str) -> Value {
    let mut hit = json!({
        "kind": entry.kind(),
        "type": entry.kind(),
        "artifactId": entry.artifact_id,
        "title": entry.title(),
        "name": entry.name,
    });
    let object = hit.as_object_mut().expect("file hit object");
    let mut put = |key: &str, value: Value| {
        if !value.is_null() && value != json!([]) {
            object.insert(key.to_string(), value);
        }
    };
    if let Some(d) = &entry.datasheet {
        put("parts", json!(d.parts));
        put("maker", json!(d.maker));
        put("productType", json!(d.product_type));
        put("topology", json!(d.topology));
        put("documentType", json!(d.document_type));
        put("textArtifactId", json!(d.text_artifact_id));
    }
    put("folder", json!(entry.folder));
    put("folderId", json!(entry.folder_id));
    put("fileType", json!(entry.file_type));
    put("mimeType", json!(entry.mime_type));
    put("sizeBytes", json!(entry.size_bytes));
    put("score", json!(score));
    put("match", json!(query));
    put("matchedOn", json!(matched_on));
    let read = match (&entry.datasheet, &entry.artifact_id) {
        (Some(_), Some(id)) => format!(
            "datasheet_facts {{\"artifact\": \"{id}\"}} for its values (add \"parameter\"); read_artifact {{\"artifact_id\": \"{id}\"}} for the file"
        ),
        (Some(d), None) => format!(
            "datasheet_facts {{\"part\": \"{}\"}} for its values",
            d.parts.first().cloned().unwrap_or_default()
        ),
        (None, Some(id)) => format!("read_artifact {{\"artifact_id\": \"{id}\"}} to read it"),
        (None, None) => String::new(),
    };
    put("next", json!(read));
    hit
}

/// The file hits for a search: best match per file over every query, best
/// first, at most `limit`. Returns the hits and how many files matched.
pub(crate) fn search_file_entries(
    entries: &[FileEntry],
    queries: &[String],
    mode: &str,
    limit: usize,
) -> (Vec<Value>, usize) {
    let vocabulary = FileVocabulary::of(entries);
    let mut scored: Vec<(f64, &str, &'static str, &FileEntry)> = Vec::new();
    for entry in entries {
        let mut best: Option<(f64, &str, &'static str)> = None;
        for query in queries {
            if let Some((score, on)) = file_match(entry, query, mode, &vocabulary) {
                if best.map_or(true, |(b, _, _)| score > b) {
                    best = Some((score, query.as_str(), on));
                }
            }
        }
        if let Some((score, query, on)) = best {
            scored.push((score, query, on, entry));
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.3.name.cmp(&b.3.name)));
    let total = scored.len();
    let hits = scored
        .iter()
        .take(limit.min(MAX_FILE_HITS))
        .map(|(score, query, on, entry)| file_hit_json(entry, *score, query, on))
        .collect();
    (hits, total)
}

/// The file pass of a search tool, added to its output beside `results`
/// (`files`, `fileTotal`, and a `note` when only files matched) in a graph that
/// holds files. `includeFiles: false` skips it (the Shrubbery search routes, which read
/// `results` only). A failure to read the files never fails the search: the
/// output then says `filesError`.
pub(crate) fn add_file_hits(
    graph_id: &str,
    graph_dir: &std::path::Path,
    arguments: &Value,
    queries: &[String],
    mode: &str,
    limit: usize,
    output: &mut Value,
    what: &str,
) {
    if !crate::mcp_utils::mcp_arg_bool(arguments, &["includeFiles", "include_files"], true) {
        return;
    }
    let (hits, total) = match load_file_entries(graph_id, graph_dir) {
        // A graph without files: the output is exactly what it was.
        Ok(entries) if entries.is_empty() => return,
        Ok(entries) => search_file_entries(&entries, queries, mode, limit),
        Err(error) => {
            output["filesError"] = json!(error);
            return;
        }
    };
    let found = output["results"].as_array().map_or(0, Vec::len);
    if found == 0 && total > 0 {
        output["note"] = json!(format!(
            "No {what} matched, but {total} file{} did: see `files` (a datasheet's values come from datasheet_facts with its artifactId; read_artifact reads a file).",
            if total == 1 { "" } else { "s" }
        ));
    }
    output["files"] = json!(hits);
    output["fileTotal"] = json!(total);
}

// ------------------------------------------------------------------ mentions

fn snippet_around(content: &str, needle: &str) -> String {
    let compact: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = compact.chars().collect();
    if chars.len() <= MENTION_SNIPPET_CHARS {
        return compact;
    }
    let lower: Vec<char> = compact.to_lowercase().chars().collect();
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    let at = if needle.is_empty() || lower.len() != chars.len() {
        0
    } else {
        lower
            .windows(needle.len())
            .position(|w| w == needle.as_slice())
            .unwrap_or(0)
    };
    let start = at.saturating_sub(MENTION_SNIPPET_CHARS / 3);
    let end = (start + MENTION_SNIPPET_CHARS).min(chars.len());
    let start = end.saturating_sub(MENTION_SNIPPET_CHARS);
    format!(
        "{}{}{}",
        if start > 0 { "..." } else { "" },
        chars[start..end].iter().collect::<String>(),
        if end < chars.len() { "..." } else { "" }
    )
}

/// The words a document uses for a datasheet parameter: the light tier's keys
/// (`vin`, `vout`, `iout`, `fsw`, `iq`) by name and symbol, anything else as
/// written ("quiescent_current" as "quiescent current").
fn parameter_phrases(parameter: &str) -> Vec<String> {
    let lower = parameter.trim().to_lowercase();
    let named: &[&str] = match norm_ident(&lower).as_str() {
        "vin" | "inputvoltage" => &["input voltage", "vin"],
        "vout" | "outputvoltage" => &["output voltage", "vout"],
        "iout" | "outputcurrent" => &["output current", "iout"],
        "fsw" | "switchingfrequency" => &["switching frequency", "fsw"],
        "iq" | "quiescentcurrent" => &["quiescent current", "iq"],
        _ => &[],
    };
    let mut phrases: Vec<String> = named.iter().map(|p| p.to_string()).collect();
    let written = lower.replace(['_', '-'], " ").trim().to_string();
    if written.chars().count() >= 2 && !phrases.contains(&written) {
        phrases.push(written);
    }
    phrases
}

/// A phrase of several words anywhere in the text; a single word only whole
/// ("vin" is not in "having").
fn says_phrase(text_lower: &str, phrase: &str) -> bool {
    if phrase.contains(' ') {
        return text_lower.contains(phrase);
    }
    text_lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == phrase)
}

/// Where a part no imported datasheet matches is named: the documents whose
/// title or text says it (with the blocks that do) and the files whose name or
/// datasheet names it. `None` when nothing does.
pub(crate) fn part_mentions(
    graph_id: &str,
    graph_dir: &std::path::Path,
    part: &str,
    parameter: Option<&str>,
) -> Result<Option<Value>, String> {
    let part = part.trim();
    if part.is_empty() {
        return Ok(None);
    }
    let documents = read_graph_documents_cold(graph_dir)?;
    let queries = vec![part.to_string()];
    let hits = ranked_lexical_block_hits(&documents, &queries, None, false);
    let wanted = part.to_lowercase();
    let wanted_norm = norm_ident(part);
    let mut by_document: Vec<(String, String, Vec<Value>)> = Vec::new();
    // Title first: a document named for the part is the best lead.
    for document in &documents {
        let title = document.title.to_lowercase();
        if title.contains(&wanted)
            || (wanted_norm.len() >= 4 && norm_ident(&title).contains(&wanted_norm))
        {
            by_document.push((
                document.document_id.clone(),
                document.title.clone(),
                Vec::new(),
            ));
        }
    }
    for hit in hits.iter().filter(|hit| {
        let content = hit.content.to_lowercase();
        content.contains(&wanted)
            || (wanted_norm.len() >= 4 && norm_ident(&content).contains(&wanted_norm))
    }) {
        let index = match by_document
            .iter()
            .position(|(id, _, _)| *id == hit.document_id)
        {
            Some(index) => index,
            None => {
                by_document.push((
                    hit.document_id.clone(),
                    hit.document_title.clone(),
                    Vec::new(),
                ));
                by_document.len() - 1
            }
        };
        let blocks = &mut by_document[index].2;
        if blocks.len() < MAX_MENTION_BLOCKS_PER_DOCUMENT {
            blocks
                .push(json!({"blockId": hit.block_id, "text": snippet_around(&hit.content, part)}));
        }
    }
    let document_total = by_document.len();
    // What those documents say about the parameter asked for ("vin": their
    // "Input voltage: 1.8V - 5.5V" line), so the answer can come from them.
    let phrases = parameter.map(parameter_phrases).unwrap_or_default();
    let documents_json: Vec<Value> = by_document
        .into_iter()
        .take(MAX_MENTION_DOCUMENTS)
        .map(|(id, title, blocks)| {
            let mut document = json!({
                "kind": "document",
                "documentId": id,
                "title": title,
                "blocks": blocks,
            });
            if !phrases.is_empty() {
                let about: Vec<Value> =
                    ranked_lexical_block_hits(&documents, &phrases, Some(id.as_str()), false)
                        .iter()
                        .filter(|hit| {
                            let content = hit.content.to_lowercase();
                            phrases.iter().any(|phrase| says_phrase(&content, phrase))
                        })
                        .take(MAX_MENTION_BLOCKS_PER_DOCUMENT)
                        .map(|hit| {
                            json!({"blockId": hit.block_id, "text": snippet_around(&hit.content, &phrases[0])})
                        })
                        .collect();
                if !about.is_empty() {
                    document["aboutParameter"] = json!(about);
                }
            }
            document["next"] = json!(format!("read_document {{\"documentId\": \"{id}\"}}"));
            document
        })
        .collect();
    let entries = load_file_entries(graph_id, graph_dir)?;
    let (files, file_total) = search_file_entries(&entries, &queries, "substring", MAX_FILE_HITS);
    if document_total == 0 && file_total == 0 {
        return Ok(None);
    }
    Ok(Some(json!({
        "part": part,
        "parameter": parameter,
        "documents": documents_json,
        "documentTotal": document_total,
        "files": files,
        "fileTotal": file_total,
    })))
}

/// The sentence a miss ends with: where to read about the part instead.
pub(crate) fn mentions_sentence(part: &str, mentions: &Value) -> String {
    let mut leads: Vec<String> = Vec::new();
    for document in mentions["documents"].as_array().into_iter().flatten() {
        leads.push(format!(
            "document \"{}\" (documentId {})",
            document["title"].as_str().unwrap_or(""),
            document["documentId"].as_str().unwrap_or("")
        ));
    }
    for file in mentions["files"].as_array().into_iter().flatten() {
        leads.push(format!(
            "{} \"{}\" (artifactId {})",
            file["kind"].as_str().unwrap_or("file"),
            file["title"].as_str().unwrap_or(""),
            file["artifactId"].as_str().unwrap_or("")
        ));
    }
    let mut says: Vec<String> = Vec::new();
    for document in mentions["documents"].as_array().into_iter().flatten() {
        for block in document["aboutParameter"].as_array().into_iter().flatten() {
            says.push(format!(
                "\"{}\" ({})",
                block["text"].as_str().unwrap_or(""),
                document["title"].as_str().unwrap_or("")
            ));
        }
    }
    let mut sentence = format!(
        "{part} is named in {}: read {} for what this workspace says about it.",
        leads.join("; "),
        if leads.len() == 1 { "it" } else { "them" }
    );
    if let (Some(parameter), false) = (mentions["parameter"].as_str(), says.is_empty()) {
        sentence.push_str(&format!(" On {parameter}, they say: {}.", says.join("; ")));
    }
    sentence
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datasheet(name: &str, title: &str, parts: &[&str], topology: Option<&str>) -> FileEntry {
        FileEntry {
            artifact_id: Some(format!("art-{name}")),
            name: name.to_string(),
            folder: Some("Datasheets".to_string()),
            datasheet: Some(DatasheetCard {
                title: Some(title.to_string()),
                parts: parts.iter().map(|p| p.to_string()).collect(),
                topology: topology.map(str::to_string),
                document_type: Some("datasheet".to_string()),
                ..DatasheetCard::default()
            }),
            ..FileEntry::default()
        }
    }

    #[test]
    fn file_match_ladder_part_title_filename_words() {
        let lm = datasheet(
            "ti-lm5164_DS.pdf",
            "LM5164 100V Input, 1A Synchronous Buck DC/DC Converter",
            &["LM5164"],
            Some("buck"),
        );
        let tps = datasheet(
            "ti-tps54331_DS.pdf",
            "TPS54331 3-A, 28-V Input, Step Down DC-DC Converter With Eco-mode",
            &["TPS54331"],
            Some("buck"),
        );
        let ldo = datasheet(
            "ti-tlv755p_DS.pdf",
            "TLV755P 500mA, Low-IQ, Small-Size, Low-Dropout Regulator",
            &["TLV755P"],
            Some("linear"),
        );
        let plain = FileEntry {
            artifact_id: Some("art-notes".into()),
            name: "bench-notes.txt".into(),
            ..FileEntry::default()
        };
        let all = [ldo.clone(), lm.clone(), tps.clone(), plain.clone()];
        let v = FileVocabulary::of(&all);
        let m = |entry: &FileEntry, query: &str, mode: &str| file_match(entry, query, mode, &v);
        assert_eq!(m(&lm, "LM5164", "auto"), Some((1.0, "part")));
        assert_eq!(m(&lm, "lm 5164", "auto"), Some((1.0, "part")));
        assert_eq!(m(&lm, "LM516", "auto"), Some((0.9, "part")));
        assert_eq!(m(&lm, "ti-lm5164_DS.pdf", "exact"), Some((1.0, "exact")));
        assert_eq!(m(&lm, "ti-lm5164_DS", "auto"), Some((1.0, "exact")));
        assert_eq!(m(&lm, "synchronous buck", "auto"), Some((0.75, "title")));
        assert_eq!(
            m(&lm, "LM5164 max input voltage", "hybrid"),
            Some((0.95, "part"))
        );
        assert_eq!(m(&plain, "bench", "auto"), Some((0.85, "filename")));
        assert_eq!(m(&plain, "notes", "substring"), Some((0.7, "filename")));
        assert!(m(&plain, "notes", "exact").is_none());
        assert!(m(&lm, "buck", "substring").is_some(), "the title holds it");
        assert!(
            m(&tps, "buck", "substring").is_none(),
            "words are for the ranked modes"
        );
        // The question itself, word for word: "library" is in no file, so the
        // two bucks match on "buck" and the regulator on "regulators".
        let question = "Which buck regulators are in the library?";
        for entry in [&lm, &tps, &ldo] {
            let (score, on) = m(entry, question, "auto").expect("matches");
            assert_eq!(on, "words");
            assert!((score - 0.3).abs() < 1e-9, "{score}");
        }
        assert!(m(&plain, question, "auto").is_none());
        let (hits, total) = search_file_entries(&all, &["buck regulator".to_string()], "auto", 20);
        assert_eq!(total, 3, "{hits:?}");
        let names: Vec<&str> = hits.iter().map(|h| h["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"ti-lm5164_DS.pdf") && names.contains(&"ti-tps54331_DS.pdf"));
        assert!(!names.contains(&"bench-notes.txt"));
        let lm_hit = hits
            .iter()
            .find(|h| h["name"] == "ti-lm5164_DS.pdf")
            .unwrap();
        assert_eq!(lm_hit["kind"], "datasheet");
        assert_eq!(lm_hit["artifactId"], "art-ti-lm5164_DS.pdf");
        assert_eq!(lm_hit["parts"], serde_json::json!(["LM5164"]));
        assert_eq!(lm_hit["topology"], "buck");
        assert!(lm_hit["next"].as_str().unwrap().contains("datasheet_facts"));
        assert!(m(&plain, "converts battery voltage to 3.3V", "hybrid").is_none());
        assert!(m(&lm, "converts battery voltage to 3.3V", "hybrid").is_none());
        assert!(m(&lm, "", "auto").is_none());
    }

    #[test]
    fn parameter_phrases_and_whole_words() {
        assert_eq!(parameter_phrases("vin"), vec!["input voltage", "vin"]);
        assert_eq!(parameter_phrases("VIN"), vec!["input voltage", "vin"]);
        assert_eq!(
            parameter_phrases("quiescent_current"),
            vec!["quiescent current", "iq"]
        );
        assert_eq!(parameter_phrases("ripple"), vec!["ripple"]);
        assert!(says_phrase("input voltage: 1.8v - 5.5v", "input voltage"));
        assert!(says_phrase("vin (max) 5.5 v", "vin"));
        assert!(!says_phrase("having a vinyl case", "vin"));
    }

    #[test]
    fn stop_words_and_plurals() {
        assert_eq!(
            query_words("Which buck regulators are in the library?"),
            vec!["buck", "regulators", "library"]
        );
        assert_eq!(singular("regulators"), "regulator");
        assert_eq!(singular("ss"), "ss");
        assert_eq!(singular("class"), "class");
    }
}
