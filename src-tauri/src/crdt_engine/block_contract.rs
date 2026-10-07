//! The block contract: gardend owns the canonical block vocabulary and runs
//! one normaliser on every JSON write path (document primitives §2, first
//! slice step 4; plans/2026-09-25-document-primitives.md in the hub).
//!
//! **Vocabulary.** `NODES` and `MARKS` are v1 of the contract, derived from
//! what the web editor accepts today: the document profile's ProseMirror
//! schema, `kernelExtensions({collaborative:true})` at shrubbery-private
//! 9eece44 (`packages/editor-kernel`), including the `unsupportedBlock` /
//! `unsupportedMark` placeholders of 272912a. The dump is committed at
//! `tests/fixtures/block_contract/editor-kernel-9eece44.schema.json` and a
//! parity test asserts the node set, content expressions, attrs and marks
//! match it exactly — except `opaqueBlock`, which gardend adds and the editor
//! does not have yet (it is `render-only` until the frontend ships its node).
//! The contract is emitted content-addressed as `block-contract.v1.json`
//! (canonical key order at every depth, 2-space indent, trailing newline;
//! `sha256` over the compact canonical JSON of everything else — the shape of
//! K1's `sophia.object-kinds.v1` snapshot).
//!
//! **Normaliser.** Every node of a JSON write gets exactly one of three
//! outcomes:
//! - *canonical*: passed through untouched (a canonical document is a
//!   byte-for-byte no-op);
//! - *known-equivalent*: a deterministic rewrite — container lists
//!   (`bulletList`/`orderedList`/`taskList`, `taskItem`) become flat
//!   `listItem{listType, indent}` (the model `normalize_imported_tiptap_json`
//!   already applies to `content` strings); a block (image, math, …) inside
//!   inline content or a list item is hoisted beside it; stray inline content
//!   at block level is wrapped in a paragraph; an empty container gets an
//!   empty paragraph;
//! - *unknown*: wrapped as `opaqueBlock{originalType, originalJson, text,
//!   contractVersion}` with `originalJson` the node's exact serialization. It
//!   projects as `mdoc:OpaqueBlock` with `mdoc:originalType`. Never deleted,
//!   never guessed.
//!
//! Unknown marks are kept verbatim (losing a format loses no text). Every
//! rewrite, and every unknown mark, is reported in the write's `warnings`
//! with its node path (`content[3].content[1]`), which is the receipt.
//! Nodes the normaliser creates or moves get a block id derived from the
//! operation id and the node path, so a replayed operation produces the same
//! bytes. A container list's id moves to its first item, so it stays
//! addressable.
//!
//! **Mark order.** A text run's marks are a set, not a sequence: Y.Text keeps
//! them as an unordered attribute map and yrs hands them back in a std
//! `HashMap`, whose iteration order differs on every read. The order a caller
//! wrote them in cannot be stored, so the order a reader sees has to come from
//! the contract, and `canonical_mark_cmp` is its one definition: the
//! contract's marks in the order of `MARKS` (the editor schema's mark rank, the
//! order of the `marks` list in the emitted file), then any mark the contract
//! does not know, by name. The normaliser leaves `marks` as written (nothing is
//! lost, so there is no receipt); the projection sorts every read with
//! `canonical_mark_cmp`, so a document read twice, or written back from a read,
//! is the same JSON. Records saved before that sort may hold any order, so a
//! comparison of a stored `tiptapJson` with a fresh projection puts both sides
//! in the canonical order first (`canonical_mark_order`). (`tiptapXml` has its
//! own, older canonical order, by mark name, because it is a convergence key;
//! that one does not change.)
//!
//! Browser Y-sync updates never reach this code (see §2 of the design note).
//!
//! Declared as a child of `content_parse` so the `#[path]` test shims that
//! compile the parser standalone carry the vocabulary with it.

use std::sync::LazyLock;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// Monotonic. Nodes are never removed from a later version: a deprecated
/// node gets a known-equivalent rewrite rule instead.
pub(crate) const CONTRACT_VERSION: u64 = 1;
pub(crate) const CONTRACT_SCHEMA: &str = "sophia.block-contract.v1";
pub(crate) const OPAQUE_BLOCK: &str = "opaqueBlock";
/// Nodes gardend adds to the contract that the web editor schema does not
/// have yet. The parity test allows exactly these; the frontend clears the
/// list by shipping the node.
pub(crate) const AWAITING_EDITOR: &[&str] = &[OPAQUE_BLOCK];

/// Node types whose `data-block-id` gardend mints when it writes them (the
/// former `content_parse::BLOCK_ID_TYPES`, plus `opaqueBlock`). The editor's
/// BlockId extension also carries ids on `tableCell`, `tableHeader` and
/// `queryBlock`, which gardend never mints (`blockId: "carried"`).
pub(crate) const MINTED_BLOCK_IDS: &[&str] = &[
    "paragraph",
    "heading",
    "listItem",
    "blockquote",
    "codeBlock",
    "horizontalRule",
    "image",
    "mathBlock",
    OPAQUE_BLOCK,
];

const LIST_CONTAINERS: &[&str] = &["bulletList", "orderedList", "taskList"];
const LIST_ITEMS: &[&str] = &["listItem", "taskItem"];
const LIST_ITEM_BLOCKS: &[&str] = &["paragraph", "heading", "codeBlock", "blockquote"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    RenderOnly,
    Authorable,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::RenderOnly => "render-only",
            State::Authorable => "authorable",
        }
    }
}

pub(crate) struct NodeSpec {
    pub(crate) name: &'static str,
    pub(crate) group: Option<&'static str>,
    pub(crate) content: Option<&'static str>,
    /// ProseMirror `marks` spec: `Some("")` = no marks allowed (codeBlock).
    pub(crate) marks: Option<&'static str>,
    pub(crate) inline: bool,
    pub(crate) atom: bool,
    attrs: &'static [&'static [&'static str]],
    pub(crate) markdown: &'static str,
    pub(crate) lossless: bool,
    pub(crate) state: State,
}

pub(crate) struct MarkSpec {
    pub(crate) name: &'static str,
    pub(crate) excludes: Option<&'static str>,
    attrs: &'static [&'static str],
    pub(crate) markdown: &'static str,
    pub(crate) lossless: bool,
    pub(crate) state: State,
}

// Attribute groups the editor's global extensions add (camelCase is the
// rule for new attrs; these carry their historical data-* names).
const ID: &[&str] = &["data-block-id"];
const OUTLINE: &[&str] = &["collapsed", "indent"];
const SOURCE: &[&str] = &[
    "data-pdf-anchor",
    "data-pdf-page",
    "data-pdf-role",
    "data-source-approach",
    "data-source-format",
    "data-source-id",
    "data-source-page",
    "data-source-role",
];
const TAGS: &[&str] = &["data-tag-expirations", "data-tags"];

/// What `lossless` means in this contract.
const LOSSLESS_MEANS: &str = "markdown -> store -> markdown keeps the node, its text and its semantic attributes (level, listType, indent, checked, language, src, alt, title, href). Block ids, source/PDF provenance, tags, outline state and presentation attributes (textAlign, size, colwidth, font) are never carried by Markdown.";

pub(crate) const NODES: &[NodeSpec] = &[
    NodeSpec { name: "doc", group: None, content: Some("block+"), marks: None, inline: false, atom: false,
        attrs: &[], markdown: "document", lossless: true, state: State::Authorable },
    NodeSpec { name: "paragraph", group: Some("block"), content: Some("inline*"), marks: None, inline: false, atom: false,
        attrs: &[OUTLINE, ID, SOURCE, TAGS, &["textAlign"]], markdown: "paragraph", lossless: true, state: State::Authorable },
    NodeSpec { name: "blockquote", group: Some("block"), content: Some("block+"), marks: None, inline: false, atom: false,
        attrs: &[OUTLINE, ID, SOURCE, TAGS], markdown: "> quote", lossless: true, state: State::Authorable },
    NodeSpec { name: "hardBreak", group: Some("inline"), content: None, marks: None, inline: true, atom: false,
        attrs: &[], markdown: "backslash line break", lossless: true, state: State::Authorable },
    NodeSpec { name: "heading", group: Some("block"), content: Some("inline*"), marks: None, inline: false, atom: false,
        attrs: &[OUTLINE, ID, SOURCE, TAGS, &["level", "textAlign"]],
        markdown: "ATX heading (levels 4-6 import as 3: the editor schema has levels 1-3)", lossless: false, state: State::Authorable },
    NodeSpec { name: "horizontalRule", group: Some("block"), content: None, marks: None, inline: false, atom: false,
        attrs: &[ID, SOURCE, TAGS], markdown: "---", lossless: true, state: State::Authorable },
    NodeSpec { name: "text", group: Some("inline"), content: None, marks: None, inline: false, atom: false,
        attrs: &[], markdown: "text (escaped)", lossless: true, state: State::Authorable },
    NodeSpec { name: "codeBlock", group: Some("block"), content: Some("text*"), marks: Some(""), inline: false, atom: false,
        attrs: &[ID, SOURCE, TAGS, &["language"]], markdown: "fenced code block with language", lossless: true, state: State::Authorable },
    NodeSpec { name: "table", group: Some("block"), content: Some("tableRow+"), marks: None, inline: false, atom: false,
        attrs: &[OUTLINE, SOURCE], markdown: "GFM table (no spans; first row becomes the header; multi-block cells joined)", lossless: false, state: State::Authorable },
    NodeSpec { name: "tableRow", group: None, content: Some("(tableCell | tableHeader)*"), marks: None, inline: false, atom: false,
        attrs: &[SOURCE], markdown: "GFM table row", lossless: false, state: State::Authorable },
    NodeSpec { name: "tableHeader", group: None, content: Some("block+"), marks: None, inline: false, atom: false,
        attrs: &[&["align", "colspan", "colwidth", "rowspan"], ID, SOURCE], markdown: "GFM header cell", lossless: false, state: State::Authorable },
    NodeSpec { name: "tableCell", group: None, content: Some("block+"), marks: None, inline: false, atom: false,
        attrs: &[&["align", "colspan", "colwidth", "rowspan"], ID, SOURCE], markdown: "GFM cell", lossless: false, state: State::Authorable },
    NodeSpec { name: "listItem", group: Some("block"), content: Some("(paragraph | heading | codeBlock | blockquote)+"), marks: None, inline: false, atom: false,
        attrs: &[&["checked", "listType"], OUTLINE, ID, SOURCE, TAGS],
        markdown: "flat list item: - / 1. / - [ ] nested by indent (ordered start renumbers from 1)", lossless: true, state: State::Authorable },
    NodeSpec { name: "footnote", group: Some("inline"), content: None, marks: None, inline: true, atom: true,
        attrs: &[&["content"]], markdown: "[^n] + definition (imports as a link)", lossless: false, state: State::Authorable },
    NodeSpec { name: "image", group: Some("block"), content: None, marks: None, inline: false, atom: true,
        attrs: &[&["alt", "size", "src", "title"], ID, SOURCE, TAGS], markdown: "![alt](src \"title\")", lossless: true, state: State::Authorable },
    NodeSpec { name: "calendarEvent", group: Some("block"), content: None, marks: None, inline: false, atom: true,
        attrs: &[&["allDay", "annotation", "externalEventId", "id", "location", "source", "timeEnd", "timeStart", "title"]],
        markdown: "(not exported)", lossless: false, state: State::Authorable },
    NodeSpec { name: "queryBlock", group: Some("block"), content: None, marks: None, inline: false, atom: true,
        attrs: &[&["collapsed", "comment", "displayMode", "maxRows", "query", "vegaLiteSpec", "visualization"], ID],
        markdown: "(not exported)", lossless: false, state: State::Authorable },
    NodeSpec { name: "mathInline", group: Some("inline"), content: None, marks: None, inline: true, atom: true,
        attrs: &[&["src"]], markdown: "$src$ (imports as text)", lossless: false, state: State::Authorable },
    NodeSpec { name: "mathBlock", group: Some("block"), content: None, marks: None, inline: false, atom: true,
        attrs: &[ID, SOURCE, TAGS, &["src"]], markdown: "$$ src $$ (imports as text)", lossless: false, state: State::Authorable },
    NodeSpec { name: "wikilink", group: Some("inline"), content: None, marks: None, inline: true, atom: true,
        attrs: &[&["blockPreview", "label", "targetBlockId", "targetDocId", "targetGraphId", "wireId"]],
        markdown: "[[label]] (target not carried)", lossless: false, state: State::Authorable },
    NodeSpec { name: "citation", group: Some("inline"), content: None, marks: None, inline: true, atom: true,
        attrs: &[&["artifactId", "citation", "zoteroKey"]], markdown: "(not exported)", lossless: false, state: State::Authorable },
    NodeSpec { name: "tagChip", group: Some("inline"), content: None, marks: None, inline: true, atom: true,
        attrs: &[&["date", "name"]], markdown: "#name (imports as text)", lossless: false, state: State::Authorable },
    NodeSpec { name: "unsupportedBlock", group: Some("block"), content: None, marks: None, inline: false, atom: true,
        attrs: &[&["nodeName", "text", "yToken"]],
        markdown: "(never stored: the editor binding's placeholder for a Y element its schema rejects)", lossless: false, state: State::RenderOnly },
    NodeSpec { name: OPAQUE_BLOCK, group: Some("block"), content: None, marks: None, inline: false, atom: true,
        attrs: &[&["contractVersion", "originalJson", "originalType", "text"], ID],
        markdown: "```json fenced block holding originalJson verbatim", lossless: true, state: State::RenderOnly },
];

/// The order of this list is the canonical mark order (`canonical_mark_cmp`):
/// it is the editor schema's mark rank, and it is what the emitted file's
/// `marks` list states. A new mark is appended in the schema's rank position.
pub(crate) const MARKS: &[MarkSpec] = &[
    MarkSpec { name: "link", excludes: None, attrs: &["class", "href", "rel", "target", "title"],
        markdown: "[text](href \"title\")", lossless: true, state: State::Authorable },
    MarkSpec { name: "textStyle", excludes: None, attrs: &["fontFamily", "fontSize"],
        markdown: "(text only)", lossless: false, state: State::Authorable },
    MarkSpec { name: "bold", excludes: None, attrs: &[], markdown: "**text**", lossless: true, state: State::Authorable },
    MarkSpec { name: "code", excludes: Some("_"), attrs: &[], markdown: "`text`", lossless: true, state: State::Authorable },
    MarkSpec { name: "italic", excludes: None, attrs: &[], markdown: "*text*", lossless: true, state: State::Authorable },
    MarkSpec { name: "strike", excludes: None, attrs: &[], markdown: "~~text~~", lossless: true, state: State::Authorable },
    MarkSpec { name: "underline", excludes: None, attrs: &[], markdown: "(text only)", lossless: false, state: State::Authorable },
    MarkSpec { name: "highlight", excludes: None, attrs: &[], markdown: "(text only)", lossless: false, state: State::Authorable },
    MarkSpec { name: "commentMark", excludes: None, attrs: &["commentId"], markdown: "(text only)", lossless: false, state: State::Authorable },
    MarkSpec { name: "unsupportedMark", excludes: Some(""), attrs: &["name", "value"],
        markdown: "(text only; the editor binding's placeholder for an unknown mark)", lossless: false, state: State::RenderOnly },
];

pub(crate) fn node_spec(name: &str) -> Option<&'static NodeSpec> {
    NODES.iter().find(|spec| spec.name == name)
}

pub(crate) fn mark_spec(name: &str) -> Option<&'static MarkSpec> {
    MARKS.iter().find(|spec| spec.name == name)
}

/// Where a mark name sits in the canonical mark order: its index in `MARKS`
/// for a contract mark, past the end of the list for every other name.
fn mark_rank(name: &str) -> usize {
    MARKS
        .iter()
        .position(|spec| spec.name == name)
        .unwrap_or(MARKS.len())
}

/// The canonical order of a text run's marks, as a comparison of two mark
/// names: contract marks in the order of `MARKS`, then every other mark by name.
/// A total order, so any permutation of the same marks sorts to the same
/// sequence. Every reader that turns Y text attributes back into a `marks`
/// array must sort with this: without it two reads of one document disagree,
/// and a stored record stops matching a fresh projection of its own Y.Doc.
pub(crate) fn canonical_mark_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    mark_rank(a).cmp(&mark_rank(b)).then_with(|| a.cmp(b))
}

/// A copy of a TipTap JSON tree in which every node's `marks` array is in the
/// canonical mark order: a stable sort of the array by `canonical_mark_cmp` on
/// each mark's `type` (a mark with no string `type` sorts as the empty name,
/// among the marks the contract does not know). Nothing else changes, byte for
/// byte. The walk follows the node tree (`content` arrays) only, so a `marks`
/// key inside a node's `attrs` is data and is left exactly as written.
///
/// This is for comparing documents, never for storing them. A record saved
/// before `marks_from_attrs` sorted its reads may hold a run's marks in any
/// order (yrs returned them in `HashMap` order), while a fresh projection of
/// the same Y.Doc holds them in the contract's order. Putting both sides in the
/// canonical order first makes the comparison about the set of marks (and their
/// attrs), so an old record still recognises its own Y.Doc, and a later change
/// to the canonical order cannot strand the records written under the old one.
pub(crate) fn canonical_mark_order(value: &Value) -> Value {
    match value {
        Value::Object(node) => {
            let mut out = Map::new();
            for (key, child) in node {
                let child = match (key.as_str(), child) {
                    ("marks", Value::Array(marks)) => {
                        let mut marks = marks.clone();
                        marks.sort_by(|a, b| {
                            canonical_mark_cmp(type_of(a).unwrap_or(""), type_of(b).unwrap_or(""))
                        });
                        Value::Array(marks)
                    }
                    ("content", Value::Array(children)) => {
                        Value::Array(children.iter().map(canonical_mark_order).collect())
                    }
                    _ => child.clone(),
                };
                out.insert(key.clone(), child);
            }
            Value::Object(out)
        }
        Value::Array(nodes) => Value::Array(nodes.iter().map(canonical_mark_order).collect()),
        other => other.clone(),
    }
}

pub(crate) fn is_known_node(name: &str) -> bool {
    node_spec(name).is_some()
}

pub(crate) fn is_block_id_minted(name: &str) -> bool {
    MINTED_BLOCK_IDS.contains(&name)
}

impl NodeSpec {
    /// The node's allowed attrs, sorted.
    pub(crate) fn attr_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.attrs.iter().flat_map(|g| g.iter().copied()).collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    fn block_id_rule(&self) -> &'static str {
        if is_block_id_minted(self.name) {
            "minted"
        } else if self.attr_names().contains(&"data-block-id") {
            "carried"
        } else {
            "none"
        }
    }
}

impl MarkSpec {
    pub(crate) fn attr_names(&self) -> Vec<&'static str> {
        let mut names = self.attrs.to_vec();
        names.sort_unstable();
        names
    }
}

/// Allowed attrs for a node type, or None for a type outside the contract.
pub(crate) fn allowed_attrs(name: &str) -> Option<Vec<&'static str>> {
    node_spec(name).map(NodeSpec::attr_names)
}

// ---------------------------------------------------------------------------
// The emitted, content-addressed contract document
// ---------------------------------------------------------------------------

fn contract_content() -> Value {
    let nodes: Vec<Value> = NODES
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "group": spec.group,
                "content": spec.content,
                "marks": spec.marks,
                "inline": spec.inline,
                "atom": spec.atom,
                "attrs": spec.attr_names(),
                "blockId": spec.block_id_rule(),
                "markdown": {"form": spec.markdown, "lossless": spec.lossless},
                "state": spec.state.as_str(),
            })
        })
        .collect();
    let marks: Vec<Value> = MARKS
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "excludes": spec.excludes,
                "attrs": spec.attr_names(),
                "markdown": {"form": spec.markdown, "lossless": spec.lossless},
                "state": spec.state.as_str(),
            })
        })
        .collect();
    json!({
        "schema": CONTRACT_SCHEMA,
        "contractVersion": CONTRACT_VERSION,
        "derivedFrom": "shrubbery-private 9eece44 packages/editor-kernel kernelExtensions({collaborative:true}) document profile, plus opaqueBlock",
        "losslessMeans": LOSSLESS_MEANS,
        "unknownMarks": "preserved verbatim, never wrapped",
        "opaqueBlock": {
            "attrs": {"originalType": "the unknown node's type", "originalJson": "the node's exact JSON serialization, kept byte-exact", "text": "plain text for display", "contractVersion": "the contract version that wrapped it"},
            "rdfType": "http://mnemosyne.dev/doc#OpaqueBlock",
        },
        "nodes": nodes,
        "marks": marks,
    })
}

/// Object keys sorted at every depth; array order kept.
pub(crate) fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), canonical_json(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

static CONTRACT: LazyLock<Value> = LazyLock::new(|| {
    let content = canonical_json(&contract_content());
    let digest = sha256_hex(serde_json::to_string(&content).expect("contract serializes").as_bytes());
    let mut snapshot = content.as_object().cloned().expect("contract is an object");
    snapshot.insert("sha256".into(), json!(digest));
    canonical_json(&Value::Object(snapshot))
});

/// The contract with its sha256 (canonical key order).
pub(crate) fn contract_snapshot() -> &'static Value {
    &CONTRACT
}

pub(crate) fn contract_sha256() -> &'static str {
    CONTRACT["sha256"].as_str().expect("contract carries sha256")
}

/// The committed bytes of `block-contract.v1.json`.
pub(crate) fn contract_file_text() -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(contract_snapshot()).expect("contract serializes")
    )
}

/// `{contractVersion, sha256}` for status surfaces.
pub(crate) fn contract_status() -> Value {
    json!({"schema": CONTRACT_SCHEMA, "contractVersion": CONTRACT_VERSION, "sha256": contract_sha256()})
}

// ---------------------------------------------------------------------------
// The normaliser
// ---------------------------------------------------------------------------

/// Normalise a document's top-level block list. Returns the normalised blocks
/// and the receipts (`warnings`). `seed` must be stable for the write (the
/// operation id) so minted block ids replay identically.
pub(crate) fn normalise_doc_content(nodes: &[Value], seed: &str) -> (Vec<Value>, Vec<String>) {
    normalise_doc_content_at(nodes, seed, "content")
}

/// As `normalise_doc_content`, with a caller-chosen path prefix
/// (`blocks` for a blocks array).
pub(crate) fn normalise_doc_content_at(
    nodes: &[Value],
    seed: &str,
    prefix: &str,
) -> (Vec<Value>, Vec<String>) {
    let mut ctx = Normaliser { seed, warnings: Vec::new(), rewrites: 0, minted: 0 };
    let blocks = ctx.block_seq(nodes, prefix);
    (blocks, ctx.warnings)
}

/// True for a warning that records a rewrite (not a preserved note).
pub(crate) fn is_rewrite_warning(warning: &str) -> bool {
    warning.starts_with("block-contract known-equivalent") || warning.starts_with("block-contract unknown")
}

struct Normaliser<'a> {
    seed: &'a str,
    warnings: Vec<String>,
    rewrites: usize,
    minted: u64,
}

fn type_of(node: &Value) -> Option<&str> {
    node.get("type").and_then(Value::as_str)
}

fn children_of(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn is_inline_type(name: &str) -> bool {
    name == "text" || node_spec(name).is_some_and(|spec| spec.group == Some("inline"))
}

fn existing_block_id(node: &Value) -> Option<String> {
    let attrs = node.get("attrs")?.as_object()?;
    ["data-block-id", "blockId"].iter().find_map(|key| {
        attrs
            .get(*key)
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    })
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty() && s != "false",
        _ => true,
    }
}

fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn number_of(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|n: &f64| n.is_finite())
}

/// Plain text of any node, for the opaque object's display text: inline runs
/// concatenate, block children are separated by newlines.
fn display_text(node: &Value) -> String {
    match type_of(node) {
        Some("text") => node.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
        Some("hardBreak") => "\n".to_string(),
        _ => {
            let children = children_of(node);
            if children.is_empty() {
                return super::tiptap_node_text(node);
            }
            let inline = children.iter().all(|c| type_of(c).is_some_and(is_inline_type));
            let parts: Vec<String> = children.iter().map(display_text).collect();
            if inline {
                parts.concat()
            } else {
                parts.into_iter().filter(|p| !p.trim().is_empty()).collect::<Vec<_>>().join("\n")
            }
        }
    }
}

/// A textblock split around a hoisted block.
enum Piece {
    Inline(Value),
    Blocks(Vec<Value>),
}

impl Normaliser<'_> {
    fn rewrite(&mut self, kind: &str, path: &str, message: String) {
        self.rewrites += 1;
        self.warnings.push(format!("block-contract {kind} {path}: {message}"));
    }

    fn note(&mut self, path: &str, message: String) {
        self.warnings.push(format!("block-contract preserved {path}: {message}"));
    }

    fn mint(&mut self, path: &str) -> String {
        let ordinal = self.minted;
        self.minted += 1;
        super::deterministic_block_id(&format!("{}#block-contract:{path}", self.seed), ordinal)
    }

    /// Give a node the normaliser created or moved a block id, when its type
    /// is one gardend mints and it has none.
    fn ensure_id(&mut self, node: &mut Value, path: &str) {
        let Some(name) = type_of(node) else { return };
        if !is_block_id_minted(name) || existing_block_id(node).is_some() {
            return;
        }
        let id = self.mint(path);
        if let Some(obj) = node.as_object_mut() {
            if !matches!(obj.get("attrs"), Some(Value::Object(_))) {
                obj.insert("attrs".into(), json!({}));
            }
            if let Some(Value::Object(attrs)) = obj.get_mut("attrs") {
                attrs.insert("data-block-id".into(), json!(id));
            }
        }
    }

    fn opaque(&mut self, node: &Value, path: &str, why: &str) -> Value {
        let original_type = type_of(node).unwrap_or("").to_string();
        let original_json = serde_json::to_string(node).expect("JSON value serializes");
        let text = display_text(node);
        let id = match existing_block_id(node) {
            Some(id) => id,
            None => self.mint(path),
        };
        let label = if original_type.is_empty() { "(untyped node)".to_string() } else { original_type.clone() };
        self.rewrite(
            "unknown",
            path,
            format!("{label} {why}; wrapped as opaqueBlock (originalJson kept byte-exact)"),
        );
        json!({
            "type": OPAQUE_BLOCK,
            "attrs": {
                "data-block-id": id,
                "originalType": original_type,
                "originalJson": original_json,
                "text": text,
                "contractVersion": CONTRACT_VERSION,
            },
        })
    }

    /// A `block+` sequence (doc, blockquote, table cells).
    fn block_seq(&mut self, nodes: &[Value], base: &str) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let mut run: Vec<Value> = Vec::new();
        let mut run_path = String::new();
        for (index, node) in nodes.iter().enumerate() {
            let path = format!("{base}[{index}]");
            if type_of(node).is_some_and(is_inline_type) {
                if run.is_empty() {
                    run_path = path.clone();
                }
                self.check_inline(node, &path);
                run.push(node.clone());
                continue;
            }
            self.flush_run(&mut run, &run_path, &mut out);
            out.extend(self.block(node, &path));
        }
        self.flush_run(&mut run, &run_path, &mut out);
        out
    }

    fn flush_run(&mut self, run: &mut Vec<Value>, path: &str, out: &mut Vec<Value>) {
        if run.is_empty() {
            return;
        }
        let content: Vec<Value> = std::mem::take(run);
        self.rewrite(
            "known-equivalent",
            path,
            format!("{} inline node(s) at block level wrapped in a paragraph", content.len()),
        );
        let mut paragraph = json!({"type": "paragraph", "content": content});
        self.ensure_id(&mut paragraph, path);
        out.push(paragraph);
    }

    /// One node in block position.
    fn block(&mut self, node: &Value, path: &str) -> Vec<Value> {
        if !node.is_object() {
            return vec![self.opaque(node, path, "is not a JSON object")];
        }
        let Some(name) = type_of(node) else {
            return vec![self.opaque(node, path, "has no type")];
        };
        if node.get("content").is_some_and(|c| !c.is_array()) {
            return vec![self.opaque(node, path, "has non-array content")];
        }
        if LIST_CONTAINERS.contains(&name) {
            return self.list_container(node, path, list_type_for_container(name), 0.0);
        }
        if LIST_ITEMS.contains(&name) {
            let list_type = attr_str(node, &["listType", "data-list-type"])
                .unwrap_or(if name == "taskItem" { "task" } else { "bullet" })
                .to_string();
            let indent = number_of(attr(node, &["indent", "data-indent"])).unwrap_or(0.0);
            return self.list_item(node, path, &list_type, indent, true, None);
        }
        if name == "doc" {
            self.rewrite("known-equivalent", path, "nested doc spliced into its parent".into());
            return self.block_seq(children_of(node), &format!("{path}.content"));
        }
        let Some(spec) = node_spec(name) else {
            return vec![self.opaque(node, path, "is not in the block contract")];
        };
        if spec.group != Some("block") {
            return vec![self.opaque(node, path, "is not allowed in block position")];
        }
        match spec.content {
            None => {
                if children_of(node).is_empty() {
                    vec![node.clone()]
                } else {
                    vec![self.opaque(node, path, "is a leaf node carrying content")]
                }
            }
            Some("inline*") => self.textblock(node, path),
            Some("text*") => self.code_block(node, path),
            Some("block+") => vec![self.container(node, path)],
            Some("tableRow+") => vec![self.table(node, path)],
            Some(_) => vec![node.clone()],
        }
    }

    fn check_inline(&mut self, node: &Value, path: &str) {
        if type_of(node) != Some("text") {
            return;
        }
        for mark in node.get("marks").and_then(Value::as_array).into_iter().flatten() {
            let name = type_of(mark).unwrap_or("");
            if mark_spec(name).is_none() {
                self.note(path, format!("unknown mark {name} kept verbatim"));
            }
        }
    }

    /// paragraph / heading: inline children only; a block child splits the
    /// textblock and is hoisted between the halves.
    fn textblock(&mut self, node: &Value, path: &str) -> Vec<Value> {
        let before = self.rewrites;
        let parent = type_of(node).unwrap_or("paragraph").to_string();
        let mut pieces: Vec<Piece> = Vec::new();
        for (index, child) in children_of(node).iter().enumerate() {
            let child_path = format!("{path}.content[{index}]");
            match type_of(child) {
                Some(name) if is_inline_type(name) => {
                    self.check_inline(child, &child_path);
                    pieces.push(Piece::Inline(child.clone()));
                }
                other => {
                    let label = other.unwrap_or("(untyped node)").to_string();
                    let mut hoisted = self.block(child, &child_path);
                    for block in &mut hoisted {
                        self.ensure_id(block, &child_path);
                    }
                    self.rewrite(
                        "known-equivalent",
                        &child_path,
                        format!("{label} hoisted out of {parent} (block content cannot sit inline)"),
                    );
                    pieces.push(Piece::Blocks(hoisted));
                }
            }
        }
        if self.rewrites == before {
            return vec![node.clone()];
        }
        let mut out: Vec<Value> = Vec::new();
        let mut current: Vec<Value> = Vec::new();
        let mut first = true;
        let mut part = 0usize;
        let mut emit = |this: &mut Self, current: &mut Vec<Value>, first: &mut bool, out: &mut Vec<Value>| {
            if !*first && current.is_empty() {
                return;
            }
            let mut piece = node.as_object().cloned().unwrap_or_default();
            let content = std::mem::take(current);
            if content.is_empty() {
                piece.remove("content");
            } else {
                piece.insert("content".into(), Value::Array(content));
            }
            let mut value = Value::Object(piece);
            if !*first {
                if let Some(Value::Object(attrs)) = value.get_mut("attrs") {
                    attrs.remove("data-block-id");
                    attrs.remove("blockId");
                }
                part += 1;
                this.ensure_id(&mut value, &format!("{path}#part{part}"));
            }
            *first = false;
            out.push(value);
        };
        for piece in pieces {
            match piece {
                Piece::Inline(value) => current.push(value),
                Piece::Blocks(blocks) => {
                    emit(self, &mut current, &mut first, &mut out);
                    out.extend(blocks);
                }
            }
        }
        emit(self, &mut current, &mut first, &mut out);
        out
    }

    /// codeBlock: text only. A hardBreak becomes a newline; anything else
    /// makes the block opaque.
    fn code_block(&mut self, node: &Value, path: &str) -> Vec<Value> {
        let mut rewritten = false;
        let mut content: Vec<Value> = Vec::new();
        for (index, child) in children_of(node).iter().enumerate() {
            let child_path = format!("{path}.content[{index}]");
            match type_of(child) {
                Some("text") => {
                    if child.get("marks").and_then(Value::as_array).is_some_and(|m| !m.is_empty()) {
                        self.note(&child_path, "marks inside codeBlock kept verbatim".into());
                    }
                    content.push(child.clone());
                }
                Some("hardBreak") => {
                    self.rewrite("known-equivalent", &child_path, "hardBreak in codeBlock stored as a newline".into());
                    rewritten = true;
                    content.push(json!({"type": "text", "text": "\n"}));
                }
                _ => return vec![self.opaque(node, path, "holds non-text content in a codeBlock")],
            }
        }
        if !rewritten {
            return vec![node.clone()];
        }
        let mut out = node.as_object().cloned().unwrap_or_default();
        out.insert("content".into(), Value::Array(content));
        vec![Value::Object(out)]
    }

    /// `block+` containers (blockquote, table cells).
    fn container(&mut self, node: &Value, path: &str) -> Value {
        let before = self.rewrites;
        let mut content = self.block_seq(children_of(node), &format!("{path}.content"));
        if content.is_empty() {
            self.rewrite(
                "known-equivalent",
                path,
                format!("empty {} given an empty paragraph", type_of(node).unwrap_or("container")),
            );
            let mut paragraph = json!({"type": "paragraph"});
            self.ensure_id(&mut paragraph, &format!("{path}.content[0]"));
            content.push(paragraph);
        }
        if self.rewrites == before {
            return node.clone();
        }
        let mut out = node.as_object().cloned().unwrap_or_default();
        out.insert("content".into(), Value::Array(content));
        Value::Object(out)
    }

    /// table > tableRow > (tableCell | tableHeader) > block+. A structural
    /// violation makes the whole table opaque (no guessing at tables).
    fn table(&mut self, node: &Value, path: &str) -> Value {
        let rows = children_of(node);
        let structural = !rows.is_empty()
            && rows.iter().all(|row| {
                type_of(row) == Some("tableRow")
                    && row.get("content").is_none_or(Value::is_array)
                    && children_of(row).iter().all(|cell| {
                        matches!(type_of(cell), Some("tableCell") | Some("tableHeader"))
                            && cell.get("content").is_none_or(Value::is_array)
                    })
            });
        if !structural {
            return self.opaque(node, path, "does not have table > tableRow > cell structure");
        }
        let before = self.rewrites;
        let mut new_rows: Vec<Value> = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            let row_path = format!("{path}.content[{r}]");
            let row_before = self.rewrites;
            let cells: Vec<Value> = children_of(row)
                .iter()
                .enumerate()
                .map(|(c, cell)| self.container(cell, &format!("{row_path}.content[{c}]")))
                .collect();
            if self.rewrites == row_before {
                new_rows.push(row.clone());
            } else {
                let mut out = row.as_object().cloned().unwrap_or_default();
                out.insert("content".into(), Value::Array(cells));
                new_rows.push(Value::Object(out));
            }
        }
        if self.rewrites == before {
            return node.clone();
        }
        let mut out = node.as_object().cloned().unwrap_or_default();
        out.insert("content".into(), Value::Array(new_rows));
        Value::Object(out)
    }

    /// bulletList / orderedList / taskList → flat listItems. The container's
    /// id moves to its first item (when that item has none of its own).
    fn list_container(&mut self, node: &Value, path: &str, list_type: &str, indent: f64) -> Vec<Value> {
        let name = type_of(node).unwrap_or("list").to_string();
        let mut inherit = existing_block_id(node);
        let mut first_item = true;
        let mut out: Vec<Value> = Vec::new();
        let slot = self.warnings.len();
        for (index, child) in children_of(node).iter().enumerate() {
            let child_path = format!("{path}.content[{index}]");
            match type_of(child) {
                Some(child_type) if LIST_ITEMS.contains(&child_type) => {
                    let item_type = if child_type == "taskItem" { "task" } else { list_type };
                    // The container's id goes to its first item, if that item has none.
                    let offered = if first_item && existing_block_id(child).is_none() {
                        inherit.take()
                    } else {
                        None
                    };
                    first_item = false;
                    out.extend(self.list_item(child, &child_path, item_type, indent, false, offered));
                }
                // A container directly inside a container stays at this level.
                Some(child_type) if LIST_CONTAINERS.contains(&child_type) => {
                    out.extend(self.list_container(child, &child_path, list_type_for_container(child_type), indent));
                }
                _ => {
                    let mut blocks = self.block(child, &child_path);
                    for block in &mut blocks {
                        self.ensure_id(block, &child_path);
                    }
                    out.extend(blocks);
                }
            }
        }
        if out.is_empty() {
            // An empty container keeps its place (and id) as one empty item.
            let mut item = json!({"type": "listItem", "attrs": {"listType": list_type, "indent": number(indent)},
                                  "content": [{"type": "paragraph"}]});
            if let (Some(id), Some(Value::Object(attrs))) = (inherit.take(), item.get_mut("attrs")) {
                attrs.insert("data-block-id".into(), json!(id));
            }
            self.ensure_id(&mut item, path);
            out.push(item);
        }
        if let Some(id) = inherit {
            self.note(path, format!("{name} id {id} not kept: its first item carries its own id"));
        }
        let items = out.iter().filter(|n| type_of(n) == Some("listItem")).count();
        // The receipt for the container precedes the receipts of its items.
        self.rewrites += 1;
        self.warnings.insert(
            slot,
            format!("block-contract known-equivalent {path}: {name} flattened to {items} listItem block(s)"),
        );
        out
    }

    /// One list item → a canonical flat listItem followed by its nested
    /// items and any hoisted blocks. `standalone` = the item stood in block
    /// position itself (not inside a container); a canonical standalone item
    /// passes through untouched.
    fn list_item(
        &mut self,
        node: &Value,
        path: &str,
        list_type: &str,
        indent: f64,
        standalone: bool,
        inherit_id: Option<String>,
    ) -> Vec<Value> {
        let before = self.rewrites;
        let name = type_of(node).unwrap_or("listItem").to_string();
        let mut direct: Vec<Value> = Vec::new();
        let mut after: Vec<Value> = Vec::new();
        let mut run: Vec<Value> = Vec::new();
        let mut run_path = String::new();
        for (index, child) in children_of(node).iter().enumerate() {
            let child_path = format!("{path}.content[{index}]");
            let child_type = type_of(child);
            match child_type {
                Some(t) if LIST_CONTAINERS.contains(&t) => {
                    self.flush_run(&mut run, &run_path, &mut direct);
                    after.extend(self.list_container(child, &child_path, list_type_for_container(t), indent + 1.0));
                }
                Some(t) if LIST_ITEMS.contains(&t) => {
                    self.flush_run(&mut run, &run_path, &mut direct);
                    self.rewrite("known-equivalent", &child_path, format!("{t} nested in {name} flattened"));
                    let nested_type = attr_str(child, &["listType", "data-list-type"])
                        .unwrap_or(if t == "taskItem" { "task" } else { list_type })
                        .to_string();
                    after.extend(self.list_item(child, &child_path, &nested_type, indent + 1.0, false, None));
                }
                Some(t) if is_inline_type(t) => {
                    if run.is_empty() {
                        run_path = child_path.clone();
                    }
                    self.check_inline(child, &child_path);
                    run.push(child.clone());
                }
                _ => {
                    self.flush_run(&mut run, &run_path, &mut direct);
                    for mut block in self.block(child, &child_path) {
                        if type_of(&block).is_some_and(|t| LIST_ITEM_BLOCKS.contains(&t)) {
                            direct.push(block);
                        } else {
                            let label = type_of(&block).unwrap_or("block").to_string();
                            self.rewrite(
                                "known-equivalent",
                                &child_path,
                                format!("{label} hoisted out of {name} (a list item holds paragraph, heading, codeBlock or blockquote)"),
                            );
                            self.ensure_id(&mut block, &child_path);
                            after.push(block);
                        }
                    }
                }
            }
        }
        self.flush_run(&mut run, &run_path, &mut direct);
        if standalone && name == "listItem" && self.rewrites == before && !direct.is_empty() {
            return vec![node.clone()];
        }
        if standalone && name == "taskItem" {
            self.rewrite("known-equivalent", path, "taskItem stored as listItem{listType:task}".into());
        }
        if direct.is_empty() {
            if standalone {
                self.rewrite("known-equivalent", path, "empty listItem given an empty paragraph".into());
            }
            let mut paragraph = json!({"type": "paragraph"});
            self.ensure_id(&mut paragraph, &format!("{path}.content[0]"));
            direct.push(paragraph);
        }
        let mut attrs = node.get("attrs").and_then(Value::as_object).cloned().unwrap_or_default();
        let own_type = attrs.get("listType").and_then(Value::as_str).map(str::to_string);
        let item_type = if standalone { own_type.unwrap_or_else(|| list_type.to_string()) } else { list_type.to_string() };
        attrs.insert("listType".into(), json!(item_type));
        if !standalone || !attrs.contains_key("indent") {
            attrs.insert("indent".into(), number(indent));
        }
        if item_type == "task" {
            let checked = attrs.get("checked").or_else(|| attrs.get("data-checked")).is_some_and(truthy);
            attrs.insert("checked".into(), json!(checked));
        }
        let mut item = node.as_object().cloned().unwrap_or_default();
        item.insert("type".into(), json!("listItem"));
        item.insert("attrs".into(), Value::Object(attrs));
        item.insert("content".into(), Value::Array(direct));
        let mut item = Value::Object(item);
        if existing_block_id(&item).is_none() {
            if let Some(id) = inherit_id {
                if let Some(Value::Object(attrs)) = item.get_mut("attrs") {
                    attrs.insert("data-block-id".into(), json!(id));
                }
            }
        }
        self.ensure_id(&mut item, path);
        let mut out = vec![item];
        out.extend(after);
        out
    }
}

fn attr<'a>(node: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let attrs = node.get("attrs")?.as_object()?;
    keys.iter().find_map(|key| attrs.get(*key).filter(|v| !v.is_null()))
}

fn attr_str<'a>(node: &'a Value, keys: &[&str]) -> Option<&'a str> {
    attr(node, keys).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn list_type_for_container(name: &str) -> &'static str {
    match name {
        "orderedList" => "ordered",
        "taskList" => "task",
        _ => "bullet",
    }
}
