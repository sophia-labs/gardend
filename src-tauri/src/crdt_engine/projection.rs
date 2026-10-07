//! Document projection: Y.Doc / TipTap JSON → tree, blocks, plain text, XML.
//!
//! Line-by-line port of:
//! - frontend/src/native/document-schema.ts (tree/blocks/plaintext)
//! - frontend/src/crdt/document-roundtrip.ts (yDocToTipTapJson / XML)
//!
//! Fidelity note: mark ids and fallback block ids are random in the TS
//! implementation (crypto.randomUUID()); this port mirrors that with uuid v4.
//! Comparison tests must normalize ids away.

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use uuid::Uuid;
use yrs::types::xml::{XmlFragment, XmlOut};
use yrs::types::Attrs;
use yrs::{Any, Doc, ReadTxn, Text, Transact, Xml};

const BLOCK_TAGS: &[&str] = &[
    "paragraph",
    "heading",
    "listItem",
    "blockquote",
    "codeBlock",
    "horizontalRule",
    "image",
    "mathBlock",
    "queryBlock",
    "opaqueBlock",
    "table",
    "tableRow",
    "tableHeader",
    "tableCell",
    "doc",
];

const MARK_TAGS: &[&str] = &[
    "strong",
    "bold",
    "em",
    "italic",
    "s",
    "strike",
    "code",
    "u",
    "underline",
    "a",
    "link",
    "mark",
    "highlight",
    "span",
    "textStyle",
    "commentMark",
    "wireMark",
];

const INLINE_ATOM_TAGS: &[&str] = &["footnote", "wikilink", "mathInline", "hardBreak"];

const BLOCK_PROJECTION_TAGS: &[&str] = &[
    "paragraph",
    "heading",
    "listItem",
    "blockquote",
    "codeBlock",
    "horizontalRule",
    "image",
    "mathBlock",
    "queryBlock",
    "opaqueBlock",
];

fn mark_name_to_tag(name: &str) -> &str {
    match name {
        "bold" => "strong",
        "italic" => "em",
        "strike" => "s",
        "code" => "code",
        "link" => "a",
        "highlight" => "mark",
        "textStyle" => "textStyle",
        "commentMark" => "commentMark",
        "wireMark" => "wireMark",
        other => other,
    }
}

fn mark_tag_to_type(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "strong" | "bold" => "bold",
        "em" | "italic" => "italic",
        "s" | "strike" => "strike",
        "u" | "underline" => "underline",
        "code" => "code",
        "mark" | "highlight" => "highlight",
        "a" | "link" => "link",
        "span" | "textStyle" => "textStyle",
        "commentMark" => "comment",
        "wireMark" => "wire",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq)]
enum NodeKind {
    Fragment,
    Block,
    Mark,
    Text,
}

impl NodeKind {
    fn as_str(&self) -> &'static str {
        match self {
            NodeKind::Fragment => "fragment",
            NodeKind::Block => "block",
            NodeKind::Mark => "mark",
            NodeKind::Text => "text",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct NodeAttributes {
    block_id: Option<String>,
    level: Option<f64>,
    href: Option<String>,
    target: Option<String>,
    language: Option<String>,
    checked: Option<bool>,
    footnote_content: Option<String>,
    annotation_id: Option<String>,
    wire_id: Option<String>,
    src: Option<String>,
    alt: Option<String>,
    extra: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct TreeNode {
    kind: NodeKind,
    tag_name: Option<String>,
    text_content: Option<String>,
    attributes: NodeAttributes,
    children: Vec<TreeNode>,
}

pub struct DocumentTree {
    root: TreeNode,
    doc_id: String,
}

/// Full projection snapshot of a document — mirrors
/// DocumentRoundtripSnapshot in document-roundtrip.ts.
pub struct ProjectionSnapshot {
    pub tiptap_json: Value,
    pub tree_json: Value,
    pub blocks_json: Value,
    pub body: String,
}

/// Port of yDocToTipTapJson: XmlFragment("content") → {type:"doc", content}.
pub fn ydoc_to_tiptap_json(doc: &Doc) -> Value {
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    let mut content = Vec::new();
    for child in fragment.children(&txn) {
        content.extend(xml_node_to_json(&txn, &child));
    }
    json!({ "type": "doc", "content": content })
}

/// Port of materializeDocumentYDoc (sans XML string, which callers obtain
/// via `ydoc_to_tiptap_xml`).
pub fn materialize_ydoc(doc: &Doc, doc_id: &str) -> ProjectionSnapshot {
    let tiptap_json = ydoc_to_tiptap_json(doc);
    materialize_tiptap_json(&tiptap_json, doc_id)
}

/// Port of materializeTipTapJsonDocument.
pub fn materialize_tiptap_json(tiptap_json: &Value, doc_id: &str) -> ProjectionSnapshot {
    let tree = document_tree_from_tiptap_json(tiptap_json, doc_id);
    let blocks = document_tree_to_blocks(&tree);
    let body = document_tree_plain_text(&tree);
    ProjectionSnapshot {
        tiptap_json: tiptap_json.clone(),
        tree_json: tree_to_json(&tree),
        blocks_json: Value::Array(blocks),
        body,
    }
}

/// Canonical TipTap XML for the document's `content` fragment.
///
/// This walks the Y.js XML tree itself rather than post-processing
/// `XmlFragment::get_string()`. The yrs string form writes text runs and
/// attribute values RAW, so a code block containing `<your-jwt-token>` or a
/// paragraph containing `<hum>` came out as markup, and the old
/// regex-shaped sanitizer could not tell literal text from real elements
/// (61 documents in the 2026-09-12 cutover rehearsal failed to parse). Walking
/// the tree means every text run and attribute value is escaped in its own
/// context, and an XML parser reads back exactly the text the author typed.
///
/// The shape is otherwise the one yrs produced, so existing fingerprints,
/// history comparisons, and the RDF `mnemo:tiptapXml` oracle keep meaning:
/// elements as `<tag k="v">…</tag>` with attributes sorted by key (yrs iterates
/// an unordered map; `tiptapXml` is a convergence/diff key, so the order must
/// be canonical), text-run marks as nested `<mark k="v">…</mark>` tags sorted
/// by mark name with their attributes sorted by key, empty elements written
/// as an open/close pair, and C0 control characters (illegal in XML 1.0)
/// dropped from text.
pub fn ydoc_to_tiptap_xml(doc: &Doc) -> String {
    let fragment = doc.get_or_insert_xml_fragment("content");
    let txn = doc.transact();
    let mut out = String::new();
    for child in fragment.children(&txn) {
        write_xml_node(&txn, &child, &mut out);
    }
    out
}

fn write_xml_node<T: ReadTxn>(txn: &T, node: &XmlOut, out: &mut String) {
    match node {
        XmlOut::Text(text) => write_xml_text_runs(txn, text, out),
        XmlOut::Element(element) => {
            let tag = element.tag().to_string();
            let mut attrs: Vec<(String, String)> = element
                .attributes(txn)
                .map(|(key, value)| (key.to_string(), format!("{value}")))
                .collect();
            attrs.sort_by(|a, b| a.0.cmp(&b.0));
            push_start_tag(out, &tag, &attrs);
            for child in element.children(txn) {
                write_xml_node(txn, &child, out);
            }
            push_end_tag(out, &tag);
        }
        XmlOut::Fragment(fragment) => {
            for child in fragment.children(txn) {
                write_xml_node(txn, &child, out);
            }
        }
    }
}

/// One `XmlText` node is a sequence of runs; each run's formatting attributes
/// become nested mark elements around the escaped text, exactly as yrs lays
/// them out, with deterministic order.
fn write_xml_text_runs<T: ReadTxn>(txn: &T, text: &yrs::XmlTextRef, out: &mut String) {
    for diff in text.diff(txn, yrs::types::text::YChange::identity) {
        let mut marks: Vec<(String, Vec<(String, String)>)> = Vec::new();
        if let Some(attrs) = diff.attributes.as_deref() {
            for (name, value) in attrs.iter() {
                let mut mark_attrs = Vec::new();
                if let Any::Map(map) = value {
                    for (key, inner) in map.iter() {
                        mark_attrs.push((key.to_string(), inner.to_string()));
                    }
                    mark_attrs.sort_by(|a, b| a.0.cmp(&b.0));
                }
                marks.push((name.to_string(), mark_attrs));
            }
            marks.sort_by(|a, b| a.0.cmp(&b.0));
        }
        for (name, mark_attrs) in &marks {
            push_start_tag(out, name, mark_attrs);
        }
        if let yrs::Out::Any(any) = &diff.insert {
            push_escaped_text(out, &any.to_string());
        }
        for (name, _) in marks.iter().rev() {
            push_end_tag(out, name);
        }
    }
}

fn push_start_tag(out: &mut String, tag: &str, attrs: &[(String, String)]) {
    out.push('<');
    out.push_str(tag);
    for (key, value) in attrs {
        out.push(' ');
        out.push_str(key);
        out.push_str("=\"");
        push_escaped_attr(out, value);
        out.push('"');
    }
    out.push('>');
}

fn push_end_tag(out: &mut String, tag: &str) {
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
}

fn is_xml_illegal_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{B}' | '\u{C}' | '\u{E}'..='\u{1F}')
}

fn push_escaped_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if is_xml_illegal_control(c) => {}
            other => out.push(other),
        }
    }
}

fn push_escaped_attr(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if is_xml_illegal_control(c) => {}
            other => out.push(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Y.Doc XML → TipTap JSON (shared with the wire-compat spike)
// ---------------------------------------------------------------------------

fn any_to_json(any: &Any) -> Value {
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => json!(b),
        Any::Number(n) => json!(n),
        Any::BigInt(n) => json!(n),
        Any::String(s) => json!(s.as_ref()),
        Any::Buffer(b) => json!(b.as_ref()),
        Any::Array(items) => Value::Array(items.iter().map(any_to_json).collect()),
        Any::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), any_to_json(v)))
                .collect(),
        ),
    }
}

fn marks_from_attrs(attrs: &Attrs) -> Vec<Value> {
    // A text run's formatting comes back from yrs as a std HashMap, whose
    // iteration order is different on every read of the same document. The
    // store holds the marks as a set; the order a reader sees is the contract's
    // (`canonical_mark_cmp`), so two reads always agree. A record's `tiptapJson`
    // is the identity key `projection_semantics_match` compares it with a fresh
    // projection by, and the store tests compare read-backs: both need this
    // order to be a function of the mark set alone.
    let mut entries: Vec<_> = attrs.iter().collect();
    entries.sort_by(|a, b| super::block_contract::canonical_mark_cmp(a.0, b.0));
    let mut marks = Vec::new();
    for (name, value) in entries {
        let mut mark = Map::new();
        mark.insert("type".into(), json!(name.to_string()));
        let attrs_json = any_to_json(value);
        if let Value::Object(ref obj) = attrs_json {
            if !obj.is_empty() {
                mark.insert("attrs".into(), attrs_json);
            }
        }
        marks.push(Value::Object(mark));
    }
    marks
}

fn xml_text_to_json<T: ReadTxn>(txn: &T, text: &yrs::XmlTextRef) -> Vec<Value> {
    let mut out = Vec::new();
    for diff in text.diff(txn, yrs::types::text::YChange::identity) {
        let chunk = match diff.insert {
            yrs::Out::Any(Any::String(s)) => s.to_string(),
            _ => continue, // non-text embeds don't occur in garden documents
        };
        if chunk.is_empty() {
            continue;
        }
        let mut node = Map::new();
        node.insert("type".into(), json!("text"));
        node.insert("text".into(), json!(chunk));
        if let Some(attrs) = diff.attributes.as_deref() {
            let marks = marks_from_attrs(attrs);
            if !marks.is_empty() {
                node.insert("marks".into(), Value::Array(marks));
            }
        }
        out.push(Value::Object(node));
    }
    out
}

fn xml_node_to_json<T: ReadTxn>(txn: &T, node: &XmlOut) -> Vec<Value> {
    match node {
        XmlOut::Text(text) => xml_text_to_json(txn, text),
        XmlOut::Element(element) => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!(element.tag().to_string()));
            let mut attrs = Map::new();
            for (key, value) in element.attributes(txn) {
                let json_value = match &value {
                    yrs::Out::Any(any) => any_to_json(any),
                    _ => Value::Null,
                };
                attrs.insert(key.to_string(), json_value);
            }
            if !attrs.is_empty() {
                obj.insert("attrs".into(), Value::Object(attrs));
            }
            let mut content = Vec::new();
            for child in element.children(txn) {
                content.extend(xml_node_to_json(txn, &child));
            }
            if !content.is_empty() {
                obj.insert("content".into(), Value::Array(content));
            }
            vec![Value::Object(obj)]
        }
        XmlOut::Fragment(fragment) => {
            let mut content = Vec::new();
            for child in fragment.children(txn) {
                content.extend(xml_node_to_json(txn, &child));
            }
            content
        }
    }
}

// ---------------------------------------------------------------------------
// TipTap JSON → DocumentTree (port of documentTreeFromTipTapJson)
// ---------------------------------------------------------------------------

fn document_tree_from_tiptap_json(json: &Value, doc_id: &str) -> DocumentTree {
    let children = json
        .get("content")
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().flat_map(tiptap_node_to_tree_nodes).collect())
        .unwrap_or_default();
    DocumentTree {
        doc_id: doc_id.to_string(),
        root: TreeNode {
            kind: NodeKind::Fragment,
            tag_name: None,
            text_content: None,
            attributes: NodeAttributes::default(),
            children,
        },
    }
}

fn tiptap_node_to_tree_nodes(node: &Value) -> Vec<TreeNode> {
    let node_type = node.get("type").and_then(Value::as_str);
    if node_type == Some("text") {
        let mut current = TreeNode {
            kind: NodeKind::Text,
            tag_name: None,
            text_content: Some(
                node.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            attributes: NodeAttributes::default(),
            children: Vec::new(),
        };
        if let Some(marks) = node.get("marks").and_then(Value::as_array) {
            for mark in marks {
                let mark_type = mark.get("type").and_then(Value::as_str).unwrap_or("");
                let tag_name = mark_name_to_tag(mark_type).to_string();
                let empty = Map::new();
                let attrs = mark
                    .get("attrs")
                    .and_then(Value::as_object)
                    .unwrap_or(&empty);
                current = TreeNode {
                    kind: NodeKind::Mark,
                    tag_name: Some(tag_name.clone()),
                    text_content: None,
                    attributes: attributes_from_tiptap(&tag_name, attrs),
                    children: vec![current],
                };
            }
        }
        return vec![current];
    }

    let tag_name = node_type.unwrap_or("paragraph").to_string();
    let empty = Map::new();
    let attrs = node
        .get("attrs")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let children = node
        .get("content")
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().flat_map(tiptap_node_to_tree_nodes).collect())
        .unwrap_or_default();
    vec![TreeNode {
        kind: classify_tag(&tag_name),
        tag_name: Some(tag_name.clone()),
        text_content: None,
        attributes: attributes_from_tiptap(&tag_name, attrs),
        children,
    }]
}

fn classify_tag(tag_name: &str) -> NodeKind {
    if INLINE_ATOM_TAGS.contains(&tag_name) {
        return NodeKind::Block;
    }
    if MARK_TAGS.contains(&tag_name) {
        return NodeKind::Mark;
    }
    let _ = BLOCK_TAGS; // parity with TS: everything else classifies as block
    NodeKind::Block
}

fn attributes_from_tiptap(tag_name: &str, attrs: &Map<String, Value>) -> NodeAttributes {
    const KNOWN: &[&str] = &[
        "data-block-id",
        "blockId",
        "level",
        "href",
        "target",
        "language",
        "checked",
        "data-checked",
        "content",
        "data-footnote-content",
        "commentId",
        "data-comment-id",
        "data-annotation-id",
        "wireId",
        "data-wire-id",
        "src",
        "alt",
    ];

    let mut result = NodeAttributes {
        block_id: string_value(attrs.get("data-block-id").or_else(|| attrs.get("blockId"))),
        level: number_value(attrs.get("level")),
        href: string_value(attrs.get("href")),
        target: string_value(attrs.get("target")),
        language: string_value(attrs.get("language")),
        checked: boolean_value(attrs.get("checked").or_else(|| attrs.get("data-checked"))),
        footnote_content: string_value(
            attrs
                .get("data-footnote-content")
                .or_else(|| attrs.get("content")),
        ),
        annotation_id: string_value(
            attrs
                .get("commentId")
                .or_else(|| attrs.get("data-comment-id"))
                .or_else(|| attrs.get("data-annotation-id")),
        ),
        wire_id: string_value(attrs.get("wireId").or_else(|| attrs.get("data-wire-id"))),
        src: string_value(attrs.get("src")),
        alt: string_value(attrs.get("alt")),
        extra: HashMap::new(),
    };

    for (key, value) in attrs {
        if !KNOWN.contains(&key.as_str()) && !value.is_null() {
            result.extra.insert(key.clone(), js_string(value));
        }
    }

    if tag_name == "listItem" {
        if let Some(list_type) = string_value(
            attrs
                .get("listType")
                .or_else(|| attrs.get("data-list-type")),
        ) {
            result.extra.insert("listType".into(), list_type);
        }
        if let Some(indent) = number_value(attrs.get("indent").or_else(|| attrs.get("data-indent")))
        {
            if indent > 0.0 {
                result
                    .extra
                    .insert("indent".into(), js_number_string(indent));
            }
        }
    }

    if tag_name == "wikilink" {
        for key in [
            "targetDocId",
            "targetBlockId",
            "targetGraphId",
            "label",
            "blockPreview",
            "wireId",
        ] {
            if let Some(value) = attrs.get(key) {
                if !value.is_null() {
                    result.extra.insert(key.to_string(), js_string(value));
                }
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tree → blocks (port of documentTreeToBlocks/collectBlocks)
// ---------------------------------------------------------------------------

fn document_tree_to_blocks(tree: &DocumentTree) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut indent_stack: Vec<(f64, String)> = Vec::new();
    for child in &tree.root.children {
        collect_blocks(child, &mut blocks, &mut indent_stack);
    }
    for (index, block) in blocks.iter_mut().enumerate() {
        if let Value::Object(obj) = block {
            obj.insert("order".into(), json!(index));
        }
    }
    blocks
}

fn collect_blocks(node: &TreeNode, blocks: &mut Vec<Value>, indent_stack: &mut Vec<(f64, String)>) {
    let is_projection = node.kind == NodeKind::Block
        && node
            .tag_name
            .as_deref()
            .map(|t| BLOCK_PROJECTION_TAGS.contains(&t))
            .unwrap_or(false);
    if !is_projection {
        for child in &node.children {
            collect_blocks(child, blocks, indent_stack);
        }
        return;
    }

    let block_id = node
        .attributes
        .block_id
        .clone()
        .unwrap_or_else(|| format!("block-{}", &Uuid::new_v4().simple().to_string()[..8]));
    let indent: f64 = node
        .attributes
        .extra
        .get("indent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    while indent_stack
        .last()
        .map(|(top, _)| *top >= indent)
        .unwrap_or(false)
    {
        indent_stack.pop();
    }
    let parent_id = indent_stack.last().map(|(_, id)| id.clone());
    let (content, marks) = inline_content_and_marks(node);

    blocks.push(json!({
        "id": block_id,
        "type": block_type_for_node(node),
        "content": content,
        "parentId": parent_id,
        "order": blocks.len(),
        "level": node.attributes.level.map(json_number),
        "checked": node.attributes.checked,
        "language": node.attributes.language,
        "marks": marks,
    }));

    if matches!(
        node.tag_name.as_deref(),
        Some("paragraph") | Some("heading") | Some("listItem")
    ) {
        indent_stack.push((indent, block_id));
    }
}

fn block_type_for_node(node: &TreeNode) -> &'static str {
    match node.tag_name.as_deref() {
        Some("heading") => "heading",
        Some("blockquote") => "quote",
        Some("codeBlock") => "code",
        Some("horizontalRule") => "divider",
        Some("image") => "image",
        Some("mathBlock") => "math",
        Some("queryBlock") => "query",
        Some("opaqueBlock") => "opaque",
        Some("listItem") => match node.attributes.extra.get("listType").map(String::as_str) {
            Some("ordered") => "numbered",
            Some("task") => "todo",
            _ => "bullet",
        },
        _ => "paragraph",
    }
}

struct ActiveMark {
    mark_type: &'static str,
    href: Option<String>,
    annotation_id: Option<String>,
    wire_id: Option<String>,
}

fn inline_content_and_marks(node: &TreeNode) -> (String, Vec<Value>) {
    if let Some(atom) = atom_node_text(node) {
        if !atom.is_empty() {
            return (atom, Vec::new());
        }
    }
    let mut text = String::new();
    let mut marks = Vec::new();
    let mut active: Vec<ActiveMark> = Vec::new();
    collect_inline(&node.children, &mut text, &mut marks, &mut active);
    (text, marks)
}

fn collect_inline(
    nodes: &[TreeNode],
    text: &mut String,
    marks: &mut Vec<Value>,
    active: &mut Vec<ActiveMark>,
) {
    for node in nodes {
        match node.kind {
            NodeKind::Text => {
                let start = text.chars().count();
                let chunk = node.text_content.clone().unwrap_or_default();
                text.push_str(&chunk);
                let end = start + chunk.chars().count();
                for mark in active.iter() {
                    marks.push(json!({
                        "id": mark_id(),
                        "type": mark.mark_type,
                        "href": mark.href,
                        "targetDocId": Value::Null,
                        "targetBlockId": Value::Null,
                        "targetGraphId": Value::Null,
                        "label": Value::Null,
                        "annotationId": mark.annotation_id,
                        "wireId": mark.wire_id,
                        "start": start,
                        "end": end,
                    }));
                }
            }
            NodeKind::Mark => {
                let mark_type = node.tag_name.as_deref().and_then(mark_tag_to_type);
                if let Some(mark_type) = mark_type {
                    active.push(ActiveMark {
                        mark_type,
                        href: node.attributes.href.clone(),
                        annotation_id: node.attributes.annotation_id.clone(),
                        wire_id: node.attributes.wire_id.clone(),
                    });
                    collect_inline(&node.children, text, marks, active);
                    active.pop();
                } else {
                    collect_inline(&node.children, text, marks, active);
                }
            }
            _ => {
                if node.tag_name.as_deref() == Some("hardBreak") {
                    text.push('\n');
                    continue;
                }
                if node.tag_name.as_deref() == Some("wikilink") {
                    let label = node
                        .attributes
                        .extra
                        .get("label")
                        .or_else(|| node.attributes.extra.get("blockPreview"))
                        .cloned()
                        .unwrap_or_else(|| "Untitled".to_string());
                    let start = text.chars().count();
                    text.push_str(&label);
                    marks.push(json!({
                        "id": mark_id(),
                        "type": "wikilink",
                        "start": start,
                        "end": start + label.chars().count(),
                        "targetDocId": node.attributes.extra.get("targetDocId"),
                        "targetBlockId": node.attributes.extra.get("targetBlockId"),
                        "targetGraphId": node.attributes.extra.get("targetGraphId"),
                        "label": label,
                        "wireId": node.attributes.extra.get("wireId").cloned()
                            .or_else(|| node.attributes.wire_id.clone()),
                    }));
                    continue;
                }
                if let Some(atom) = atom_node_text(node) {
                    if !atom.is_empty() {
                        text.push_str(&atom);
                        continue;
                    }
                }
                collect_inline(&node.children, text, marks, active);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Plain text (port of documentTreePlainText/plainText)
// ---------------------------------------------------------------------------

fn document_tree_plain_text(tree: &DocumentTree) -> String {
    tree.root
        .children
        .iter()
        .map(plain_text)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn plain_text(node: &TreeNode) -> String {
    if node.kind == NodeKind::Text {
        return node.text_content.clone().unwrap_or_default();
    }
    if node.tag_name.as_deref() == Some("hardBreak") {
        return "\n".to_string();
    }
    if let Some(atom) = atom_node_text(node) {
        if !atom.is_empty() {
            return atom;
        }
    }
    node.children.iter().map(plain_text).collect()
}

fn atom_node_text(node: &TreeNode) -> Option<String> {
    let attrs = &node.attributes;
    let text = match node.tag_name.as_deref() {
        Some("wikilink") => attrs
            .extra
            .get("label")
            .or_else(|| attrs.extra.get("blockPreview"))
            .cloned()
            .unwrap_or_default(),
        Some("footnote") => attrs
            .footnote_content
            .as_ref()
            .map(|c| format!("[^{c}]"))
            .unwrap_or_default(),
        Some("mathInline") | Some("mathBlock") => attrs
            .src
            .clone()
            .or_else(|| attrs.extra.get("src").cloned())
            .unwrap_or_default(),
        Some("image") => attrs
            .alt
            .clone()
            .or_else(|| attrs.extra.get("title").cloned())
            .or_else(|| attrs.src.clone())
            .unwrap_or_default(),
        // The block contract's wrapper for content it does not know: its
        // display text is the block's text.
        Some("opaqueBlock") => attrs.extra.get("text").cloned().unwrap_or_default(),
        Some("queryBlock") => {
            let parts: Vec<&String> = [attrs.extra.get("comment"), attrs.extra.get("query")]
                .into_iter()
                .flatten()
                .collect();
            parts
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    Some(text)
}

// ---------------------------------------------------------------------------
// Tree → tree.json (serialization matching the TS snapshot shape)
// ---------------------------------------------------------------------------

fn tree_to_json(tree: &DocumentTree) -> Value {
    json!({
        "docId": tree.doc_id,
        "root": tree_node_to_json(&tree.root),
    })
}

fn tree_node_to_json(node: &TreeNode) -> Value {
    json!({
        "kind": node.kind.as_str(),
        "tagName": node.tag_name,
        "textContent": node.text_content,
        "attributes": {
            "blockId": node.attributes.block_id,
            "level": node.attributes.level.map(json_number),
            "href": node.attributes.href,
            "target": node.attributes.target,
            "language": node.attributes.language,
            "checked": node.attributes.checked,
            "footnoteContent": node.attributes.footnote_content,
            "annotationId": node.attributes.annotation_id,
            "wireId": node.attributes.wire_id,
            "src": node.attributes.src,
            "alt": node.attributes.alt,
            "extra": node.attributes.extra,
        },
        "children": node.children.iter().map(tree_node_to_json).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Helpers (port of stringValue/numberValue/booleanValue + JS coercions)
// ---------------------------------------------------------------------------

/// Canonicalize the attribute order of every start-tag in a serialized TipTap
/// XML string so the output is deterministic regardless of the CRDT library's
/// internal (unordered/insertion-history) attribute-map iteration order.
///
/// CONTRACT (must match the frontend `canonicalizeTipTapXmlAttrOrder` in
/// document-roundtrip.ts byte-for-byte):
/// - Within each `<tag a="…" b="…">` start-tag, attributes are sorted
///   lexicographically by attribute NAME (key), comparing by Unicode scalar
///   value (Rust `str` `Ord` == JS `<` on the key strings, which agree for the
///   attribute names TipTap uses).
/// - The tag name, self-closing `/`, end-tags `</tag>`, text content, and the
///   escaping of attribute values are left exactly as the serializer produced
///   them — only the order of the `name="value"` tokens inside a start-tag
///   changes.
/// - Attribute VALUES are escaped by the serializer, so `>` never appears raw
///   inside a value; the first unquoted `>` therefore reliably ends the tag.
fn canonicalize_tiptap_xml_attr_order(xml: &str) -> String {
    let bytes = xml.as_bytes();
    let mut out = String::with_capacity(xml.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            // Copy text content verbatim (UTF-8 safe: advance by char).
            let ch = xml[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        // Find the end of this tag: the first '>' not inside a double-quoted
        // attribute value. (Attribute values are escaped, so a literal '>'
        // outside quotes cannot occur inside a tag.)
        let mut j = i + 1;
        let mut in_quote = false;
        while j < bytes.len() {
            match bytes[j] {
                b'"' => in_quote = !in_quote,
                b'>' if !in_quote => break,
                _ => {}
            }
            j += 1;
        }
        if j >= bytes.len() {
            // No tag end — copy the remainder verbatim.
            out.push_str(&xml[i..]);
            break;
        }
        out.push_str(&canonicalize_one_start_tag(&xml[i..=j]));
        i = j + 1;
    }
    out
}

/// Sort the attribute tokens of one `<tag attrs...>` (or `</tag>` / `<tag/>`)
/// by attribute key, leaving tag name and punctuation in place.
fn canonicalize_one_start_tag(tag: &str) -> String {
    // Strip surrounding `<` `>`.
    let inner = &tag[1..tag.len() - 1];
    // End-tags have no attributes — leave untouched.
    if inner.starts_with('/') {
        return tag.to_string();
    }
    let (inner, trailing_slash) = if let Some(stripped) = inner.strip_suffix('/') {
        (stripped.trim_end(), true)
    } else {
        (inner, false)
    };
    let mut parts = inner.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("");
    let attr_blob = parts.next().unwrap_or("").trim();
    if attr_blob.is_empty() {
        return if trailing_slash {
            format!("<{name}/>")
        } else {
            format!("<{name}>")
        };
    }
    // Tokenize `name="value"` pairs, respecting quotes.
    let mut attrs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in attr_blob.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                cur.push(ch);
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    attrs.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        attrs.push(cur);
    }
    // Sort by attribute KEY (text before the first '='), preserving value.
    attrs.sort_by(|a, b| attr_key(a).cmp(attr_key(b)));
    let joined = attrs.join(" ");
    if trailing_slash {
        format!("<{name} {joined}/>")
    } else {
        format!("<{name} {joined}>")
    }
}

/// The attribute name of a `name="value"` token (text before the first '=').
fn attr_key(token: &str) -> &str {
    token.split_once('=').map(|(k, _)| k).unwrap_or(token)
}

fn mark_id() -> String {
    Uuid::new_v4().to_string()[..12].to_string()
}

fn string_value(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(v) => Some(js_string(v)),
    }
}

fn number_value(value: Option<&Value>) -> Option<f64> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn boolean_value(value: Option<&Value>) -> Option<bool> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(v) => Some(matches!(
            js_string(v).to_lowercase().as_str(),
            "true" | "1" | "yes"
        )),
    }
}

/// JS String(value) coercion, exposed for sibling engine modules.
pub(crate) fn js_string_pub(value: &Value) -> String {
    js_string(value)
}

/// JS String(value) coercion for the value types that occur in attrs.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => js_number_string(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn js_number_string(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn json_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

#[cfg(test)]
mod xml_escaping_tests {
    //! The 2026-09-12 rehearsal finding: literal angle brackets in text made
    //! the stored XML unparseable. The oracle here is the crate's own XML
    //! reader: what the serializer writes must read back as the same TipTap
    //! JSON the Y.Doc projects directly.
    use super::{ydoc_to_tiptap_json, ydoc_to_tiptap_xml};
    use crate::crdt_engine::builder::ydoc_from_tiptap_json;
    use crate::crdt_engine::content_parse::tiptap_xml_to_tiptap_json;
    use serde_json::{json, Value};

    fn strip_generated_ids(value: &mut Value) {
        // The reader mints ids for elements that carry none; compare structure + text.
        match value {
            Value::Object(map) => {
                if let Some(Value::Object(attrs)) = map.get_mut("attrs") {
                    attrs.remove("data-block-id");
                    attrs.remove("id");
                    if attrs.is_empty() {
                        map.remove("attrs");
                    }
                }
                for v in map.values_mut() {
                    strip_generated_ids(v);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(strip_generated_ids),
            _ => {}
        }
    }

    fn round_trips(tiptap: Value) -> String {
        let doc = ydoc_from_tiptap_json(&tiptap);
        let xml = ydoc_to_tiptap_xml(&doc);
        let mut direct = ydoc_to_tiptap_json(&doc);
        let mut via_xml = tiptap_xml_to_tiptap_json(&xml)
            .unwrap_or_else(|e| panic!("serialized XML must parse: {e}\n{xml}"));
        strip_generated_ids(&mut direct);
        strip_generated_ids(&mut via_xml);
        assert_eq!(
            direct, via_xml,
            "XML round trip changed the document\n{xml}"
        );
        xml
    }

    #[test]
    fn literal_angle_brackets_in_code_and_text_survive() {
        // api-reference and hum-bootstrap, the two named reproductions.
        let xml = round_trips(json!({"type": "doc", "content": [
            {"type": "codeBlock", "attrs": {"language": "http"}, "content": [
                {"type": "text", "text": "Authorization: Bearer <your-jwt-token>\nX-Debug: a < b && c > d"}]},
            {"type": "paragraph", "content": [
                {"type": "text", "text": "The Cantor wraps lines in "},
                {"type": "text", "text": "<hum>", "marks": [{"type": "code"}]},
                {"type": "text", "text": " tags."}]},
        ]}));
        assert!(xml.contains("Bearer &lt;your-jwt-token&gt;"), "{xml}");
        assert!(xml.contains("<code>&lt;hum&gt;</code>"), "{xml}");
        assert!(!xml.contains("<your-jwt-token>"), "{xml}");
    }

    #[test]
    fn ampersands_and_entity_lookalikes_are_literal_text() {
        // Old sanitizer left "&amp;" typed by a human as-is, which read back as "&".
        let xml = round_trips(json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "Tom &amp; Jerry & co; &#42; is a star"}]}
        ]}));
        assert!(
            xml.contains("Tom &amp;amp; Jerry &amp; co; &amp;#42; is a star"),
            "{xml}"
        );
    }

    #[test]
    fn attribute_values_are_escaped_and_ordered() {
        let xml = round_trips(json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [
                {"type": "text", "text": "link", "marks": [{"type": "link", "attrs": {"target": "_blank", "href": "https://x/?a=1&b=\"q\"<c>"}}]}]}
        ]}));
        assert!(xml.contains(r#"<link href="https://x/?a=1&amp;b=&quot;q&quot;&lt;c&gt;" target="_blank">link</link>"#), "{xml}");
    }

    #[test]
    fn element_attributes_and_stacked_marks_are_canonically_ordered() {
        let doc = ydoc_from_tiptap_json(&json!({"type": "doc", "content": [
            {"type": "heading", "attrs": {"level": 2, "data-block-id": "h1"}, "content": [
                {"type": "text", "text": "hi", "marks": [{"type": "italic"}, {"type": "bold"}]}]}
        ]}));
        let xml = ydoc_to_tiptap_xml(&doc);
        assert!(
            xml.starts_with(r#"<heading data-block-id="h1" level="2">"#),
            "{xml}"
        );
        assert!(xml.contains("<bold><italic>hi</italic></bold>"), "{xml}");
    }

    #[test]
    fn control_characters_are_dropped_and_empty_elements_keep_a_pair() {
        let doc = ydoc_from_tiptap_json(&json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "a\u{0}b\u{7}c"}]},
            {"type": "paragraph"}
        ]}));
        let xml = ydoc_to_tiptap_xml(&doc);
        assert!(xml.contains(">abc</paragraph>"), "{xml}");
        assert!(xml.ends_with("<paragraph></paragraph>"), "{xml}");
        tiptap_xml_to_tiptap_json(&xml).expect("parses");
    }
}

#[cfg(test)]
mod canonical_attr_tests {
    use super::canonicalize_tiptap_xml_attr_order as canon;

    #[test]
    fn sorts_start_tag_attrs_by_key() {
        // The observed real case: <heading> with data-block-id + level in either order.
        let a = r#"<heading data-block-id="b1" level="1">Title</heading>"#;
        let b = r#"<heading level="1" data-block-id="b1">Title</heading>"#;
        let expected = r#"<heading data-block-id="b1" level="1">Title</heading>"#;
        assert_eq!(canon(a), expected);
        assert_eq!(canon(b), expected);
        // Idempotent and convergent: both inputs map to the same canonical bytes.
        assert_eq!(canon(a), canon(b));
        assert_eq!(canon(&canon(a)), canon(a));
    }

    #[test]
    fn leaves_text_and_end_tags_untouched() {
        let xml = r#"<paragraph>plain &amp; text 1 < 2</paragraph>"#;
        assert_eq!(canon(xml), xml);
    }

    #[test]
    fn handles_self_closing_and_no_attrs() {
        assert_eq!(canon("<hardBreak/>"), "<hardBreak/>");
        assert_eq!(canon("<paragraph></paragraph>"), "<paragraph></paragraph>");
        assert_eq!(canon(r#"<x b="2" a="1"/>"#), r#"<x a="1" b="2"/>"#);
    }

    #[test]
    fn does_not_split_on_spaces_inside_values() {
        let xml = r#"<a href="https://x/y z" target="_blank">k</a>"#;
        // href < target, already sorted; the space inside the value must not split.
        assert_eq!(canon(xml), xml);
        let xml2 = r#"<a target="_blank" href="https://x/y z">k</a>"#;
        assert_eq!(canon(xml2), xml);
    }

    #[test]
    fn does_not_treat_gt_inside_value_as_tag_end() {
        // The serializer escapes '>' as &gt;, so a raw '>' never appears in a value;
        // but an escaped one must be carried through and not confuse tag scanning.
        let xml = r#"<a title="a&gt;b" href="h">k</a>"#;
        let expected = r#"<a href="h" title="a&gt;b">k</a>"#;
        assert_eq!(canon(xml), expected);
    }
}

#[cfg(test)]
mod byte_stability_tests {
    use super::ydoc_to_tiptap_xml;
    use crate::crdt_engine::builder::ydoc_from_tiptap_json;
    use serde_json::json;
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, ReadTxn, Transact, Update};

    /// Re-hydrate a NEW Doc purely from `from`'s serialized update bytes. This is
    /// the SAME path `RoomRegistry`/`live_doc_from_record` take: encode the live
    /// Doc's state as a v1 update, then apply it to a blank Doc — yielding a Doc
    /// with identical CONTENT but an independent (history-free) internal layout.
    fn hydrate_from_bytes(from: &Doc) -> Doc {
        let bytes = from
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        let doc = Doc::new();
        {
            let update = Update::decode_v1(&bytes).expect("decode_v1");
            let mut txn = doc.transact_mut();
            txn.apply_update(update).expect("apply_update");
        }
        doc
    }

    /// THE convergence-key invariant: a Doc built incrementally (the live write
    /// Doc) and the SAME logical content hydrated from its persisted bytes must
    /// serialize to BYTE-IDENTICAL tiptapXml. This is the exact scenario that
    /// failed before canonicalization — `yrs` `get_string` leaked the construction
    /// history via its unordered attribute map, so the two layouts emitted
    /// start-tag attributes in different orders.
    #[test]
    fn incremental_and_hydrated_serialize_byte_equal() {
        // A document exercising EVERY non-determinism channel the recon named:
        // - multi-attribute block elements (heading: data-block-id + level),
        // - link nodes with multiple attrs (href + target),
        // - text marks with multiple inner attrs (link mark: href + target),
        //   which yrs wraps as <a …> start-tags from an unordered Any::Map.
        let tiptap = json!({
            "type": "doc",
            "content": [
                {
                    "type": "heading",
                    "attrs": { "data-block-id": "b1", "level": 1 },
                    "content": [{ "type": "text", "text": "Title" }]
                },
                {
                    "type": "paragraph",
                    "attrs": { "data-block-id": "b2" },
                    "content": [
                        { "type": "text", "text": "plain " },
                        {
                            "type": "text",
                            "text": "linked",
                            "marks": [
                                { "type": "a", "attrs": { "href": "https://x", "target": "_blank" } }
                            ]
                        }
                    ]
                }
            ]
        });

        let live = ydoc_from_tiptap_json(&tiptap);
        let hydrated = hydrate_from_bytes(&live);

        let live_xml = ydoc_to_tiptap_xml(&live);
        let hydrated_xml = ydoc_to_tiptap_xml(&hydrated);

        assert!(
            !live_xml.is_empty(),
            "non-empty serialization (meaningful check)"
        );
        assert!(live_xml.contains("<heading"), "carries the heading element");
        assert_eq!(
            live_xml, hydrated_xml,
            "BYTE-STABLE: incrementally-built and hydrated-from-bytes Docs serialize \
             to identical tiptapXml — the serializer depends only on Y.Doc CONTENT, \
             not construction history.\n  live    : {live_xml}\n  hydrated: {hydrated_xml}"
        );
    }

    /// Construction-history independence within the SAME process: build the SAME
    /// logical document twice with attributes DECLARED in opposite source orders.
    /// The canonical serializer must collapse both to the same bytes (even when the
    /// per-process HashMap seed happens to keep raw orders apart).
    #[test]
    fn attribute_declaration_order_does_not_leak() {
        let forward = json!({
            "type": "doc",
            "content": [{
                "type": "heading",
                "attrs": { "data-block-id": "b1", "level": 2 },
                "content": [{ "type": "text", "text": "H" }]
            }]
        });
        let reverse = json!({
            "type": "doc",
            "content": [{
                "type": "heading",
                "attrs": { "level": 2, "data-block-id": "b1" },
                "content": [{ "type": "text", "text": "H" }]
            }]
        });

        let a = ydoc_to_tiptap_xml(&ydoc_from_tiptap_json(&forward));
        let b = ydoc_to_tiptap_xml(&ydoc_from_tiptap_json(&reverse));
        assert_eq!(
            a, b,
            "attribute DECLARATION order must not leak into tiptapXml\n  a: {a}\n  b: {b}"
        );
        // And the canonical order is lexicographic by key (data-block-id < level).
        assert!(
            a.contains(r#"<heading data-block-id="b1" level="2">"#),
            "attributes are sorted lexicographically by name, got {a}"
        );
    }

    /// Round-trip safety: canonicalizing the XML does not corrupt the content — the
    /// JSON projection (the structural source of truth) is unchanged, and the
    /// serialized XML still parses back to the same fragment shape.
    #[test]
    fn canonicalization_preserves_content() {
        let tiptap = json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "attrs": { "z-last": "1", "a-first": "2", "m-mid": "3" },
                "content": [{ "type": "text", "text": "x" }]
            }]
        });
        let xml = ydoc_to_tiptap_xml(&ydoc_from_tiptap_json(&tiptap));
        // All three attributes survive, sorted, with their values intact.
        assert_eq!(
            xml, r#"<paragraph a-first="2" m-mid="3" z-last="1">x</paragraph>"#,
            "all attributes preserved and lexicographically ordered"
        );
    }
}
