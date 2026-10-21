//! Semantic equality, carry rules, hashing and the normal form (DESIGN §2.3).

use serde_json::{Map, Number, Value};

use super::types::{MarkSet, RawMarks, SemAttrs, SourceFormat};
use crate::crdt_engine::block_contract::canonical_mark_cmp;
use crate::crdt_engine::builder::json_to_any;

/// Block-id attributes: excluded from every hash and from attribute patches (ids follow IdPolicy).
pub(crate) const ID_ATTRS: &[&str] = &["data-block-id", "blockId"];

/// A schema default, transcribed from the editor schema (DESIGN Appendix B).
#[derive(Clone, Copy)]
enum Dv {
    Bool(bool),
    Int(i64),
    Str(&'static str),
}

impl Dv {
    fn to_value(self) -> Value {
        match self {
            Dv::Bool(b) => Value::Bool(b),
            // numbers are semantic f64 (sem_value's rule)
            Dv::Int(n) => Value::from(n as f64),
            Dv::Str(s) => Value::String(s.to_string()),
        }
    }
}

/// The 52 non-null defaults of `editor-kernel-319e0253.schema.json`; a test compares this table to the fixture
/// in both directions. Every attribute not listed defaults to null (absent).
const OUTLINE_DEFAULTS: &[(&str, Dv)] = &[("collapsed", Dv::Bool(false)), ("indent", Dv::Int(0))];
const HEADING_DEFAULTS: &[(&str, Dv)] = &[
    ("collapsed", Dv::Bool(false)),
    ("indent", Dv::Int(0)),
    ("level", Dv::Int(1)),
];
const CELL_DEFAULTS: &[(&str, Dv)] = &[("colspan", Dv::Int(1)), ("rowspan", Dv::Int(1))];
const LIST_ITEM_DEFAULTS: &[(&str, Dv)] = &[
    ("checked", Dv::Bool(false)),
    ("collapsed", Dv::Bool(false)),
    ("indent", Dv::Int(0)),
    ("listType", Dv::Str("bullet")),
];
const FOOTNOTE_DEFAULTS: &[(&str, Dv)] = &[("content", Dv::Str(""))];
const IMAGE_DEFAULTS: &[(&str, Dv)] = &[("alt", Dv::Str("")), ("size", Dv::Str("large"))];
const CALENDAR_EVENT_DEFAULTS: &[(&str, Dv)] = &[
    ("allDay", Dv::Bool(true)),
    ("annotation", Dv::Str("")),
    ("id", Dv::Str("")),
    ("location", Dv::Str("")),
    ("source", Dv::Str("manual")),
    ("title", Dv::Str("")),
];
const QUERY_BLOCK_DEFAULTS: &[(&str, Dv)] = &[
    ("collapsed", Dv::Bool(false)),
    ("comment", Dv::Str("")),
    ("displayMode", Dv::Str("auto")),
    ("maxRows", Dv::Int(100)),
    (
        "query",
        Dv::Str("SELECT ?s ?p ?o WHERE {\n  ?s ?p ?o .\n}\nLIMIT 25"),
    ),
    ("vegaLiteSpec", Dv::Str("")),
    ("visualization", Dv::Str("table")),
];
const OBJECT_EMBED_DEFAULTS: &[(&str, Dv)] = &[
    ("classLabel", Dv::Str("")),
    ("iri", Dv::Str("")),
    ("label", Dv::Str("")),
    ("slot", Dv::Str("card")),
];
const SRC_DEFAULTS: &[(&str, Dv)] = &[("src", Dv::Str(""))];
const WIKILINK_DEFAULTS: &[(&str, Dv)] = &[("label", Dv::Str("Untitled"))];
const CITATION_DEFAULTS: &[(&str, Dv)] = &[
    ("artifactId", Dv::Str("")),
    ("citation", Dv::Str("")),
    ("zoteroKey", Dv::Str("")),
];
const TAG_CHIP_DEFAULTS: &[(&str, Dv)] = &[("name", Dv::Str(""))];
const UNSUPPORTED_BLOCK_DEFAULTS: &[(&str, Dv)] =
    &[("nodeName", Dv::Str("")), ("text", Dv::Str(""))];
const OPAQUE_BLOCK_DEFAULTS: &[(&str, Dv)] = &[
    ("originalJson", Dv::Str("")),
    ("originalType", Dv::Str("")),
    ("text", Dv::Str("")),
];
const LINK_DEFAULTS: &[(&str, Dv)] = &[
    ("rel", Dv::Str("noopener noreferrer nofollow")),
    ("target", Dv::Str("_blank")),
];
const UNSUPPORTED_MARK_DEFAULTS: &[(&str, Dv)] = &[("name", Dv::Str(""))];

fn node_defaults(tag: &str) -> &'static [(&'static str, Dv)] {
    match tag {
        "paragraph" | "blockquote" | "table" => OUTLINE_DEFAULTS,
        "heading" => HEADING_DEFAULTS,
        "tableHeader" | "tableCell" => CELL_DEFAULTS,
        "listItem" => LIST_ITEM_DEFAULTS,
        "footnote" => FOOTNOTE_DEFAULTS,
        "image" => IMAGE_DEFAULTS,
        "calendarEvent" => CALENDAR_EVENT_DEFAULTS,
        "queryBlock" => QUERY_BLOCK_DEFAULTS,
        "objectEmbed" => OBJECT_EMBED_DEFAULTS,
        "mathInline" | "mathBlock" => SRC_DEFAULTS,
        "wikilink" => WIKILINK_DEFAULTS,
        "citation" => CITATION_DEFAULTS,
        "tagChip" => TAG_CHIP_DEFAULTS,
        "unsupportedBlock" => UNSUPPORTED_BLOCK_DEFAULTS,
        "opaqueBlock" => OPAQUE_BLOCK_DEFAULTS,
        _ => &[],
    }
}

fn mark_defaults(mark: &str) -> &'static [(&'static str, Dv)] {
    match mark {
        "link" => LINK_DEFAULTS,
        "unsupportedMark" => UNSUPPORTED_MARK_DEFAULTS,
        _ => &[],
    }
}

/// Every number as an f64 JSON number, so 1 and 1.0 (and a Y `Any::Number`) compare equal. -0.0 reads as 0.0.
fn sem_number(number: &Number) -> Value {
    match number.as_f64() {
        Some(f) => {
            let f = if f == 0.0 { 0.0 } else { f };
            Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or_else(|| Value::Number(number.clone()))
        }
        None => Value::Number(number.clone()),
    }
}

/// `None` for JSON null; integral and fractional numbers both as f64 JSON numbers; other values unchanged
/// (objects and arrays recursively).
pub(crate) fn sem_value(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Number(number) => Some(sem_number(number)),
        // a null inside an array is a position, not an absence: it stays
        Value::Array(items) => Some(Value::Array(
            items
                .iter()
                .map(|item| sem_value(item).unwrap_or(Value::Null))
                .collect(),
        )),
        // a null-valued key is an absent key
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                if let Some(item) = sem_value(item) {
                    out.insert(key.clone(), item);
                }
            }
            Some(Value::Object(out))
        }
        other => Some(other.clone()),
    }
}

/// Defaults first, then every non-null raw value over them (null reads as absent, so the default applies).
fn sem_attrs_with(defaults: &[(&str, Dv)], raw: Option<&Map<String, Value>>) -> SemAttrs {
    let mut out = SemAttrs::new();
    for (key, default) in defaults {
        out.insert((*key).to_string(), default.to_value());
    }
    if let Some(raw) = raw {
        for (key, value) in raw {
            if let Some(value) = sem_value(value) {
                out.insert(key.clone(), value);
            }
        }
    }
    out
}

/// Raw node attrs -> SemAttrs: the editor schema's defaults for `tag` (DESIGN Appendix B) filled in for
/// absent keys, nulls dropped, numbers normalized. ID_ATTRS are kept (callers exclude them where needed).
pub(crate) fn sem_node_attrs(tag: &str, raw: &Map<String, Value>) -> SemAttrs {
    sem_attrs_with(node_defaults(tag), Some(raw))
}

/// Raw mark attrs (an object, `{}` or absent) -> SemAttrs with the mark's schema defaults.
pub(crate) fn sem_mark_attrs(mark: &str, raw: Option<&Value>) -> SemAttrs {
    // a legacy non-object value (`true`) carries no attrs
    sem_attrs_with(mark_defaults(mark), raw.and_then(Value::as_object))
}

/// A TipTap `marks` array -> (semantic, raw). Raw values are the mark's `attrs` object, `{}` when absent
/// (the shape builder.rs:80-85 writes).
pub(crate) fn marks_from_tiptap(marks: Option<&Value>) -> (MarkSet, RawMarks) {
    let mut semantic = MarkSet::new();
    let mut raw = RawMarks::new();
    let Some(Value::Array(marks)) = marks else {
        return (semantic, raw);
    };
    for mark in marks {
        let Some(name) = mark.get("type").and_then(Value::as_str) else {
            continue;
        };
        let attrs = match mark.get("attrs") {
            Some(Value::Object(attrs)) => Value::Object(attrs.clone()),
            _ => Value::Object(Map::new()),
        };
        semantic.insert(name.to_string(), sem_mark_attrs(name, Some(&attrs)));
        raw.insert(name.to_string(), attrs);
    }
    (semantic, raw)
}

fn any_to_json(any: &yrs::Any) -> Value {
    use yrs::Any;
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => Value::Bool(*b),
        Any::Number(n) => serde_json::json!(n),
        Any::BigInt(n) => serde_json::json!(n),
        Any::String(s) => Value::String(s.to_string()),
        Any::Buffer(b) => serde_json::json!(b.as_ref()),
        Any::Array(items) => Value::Array(items.iter().map(any_to_json).collect()),
        // Sorted by key: yrs keeps a map in a HashMap, and serde_json here preserves insertion order, so
        // unsorted output would differ from run to run (I10).
        Any::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), any_to_json(v)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
    }
}

/// Y formatting attributes of one text chunk -> (semantic, raw). `Any::Map` values become JSON objects;
/// any other value is kept as its JSON (a legacy `true` reads as `true`).
pub(crate) fn marks_from_y(attrs: Option<&yrs::types::Attrs>) -> (MarkSet, RawMarks) {
    let mut semantic = MarkSet::new();
    let mut raw = RawMarks::new();
    let Some(attrs) = attrs else {
        return (semantic, raw);
    };
    for (name, value) in attrs {
        // a null attribute is a removed format, not a mark
        if matches!(value, yrs::Any::Null | yrs::Any::Undefined) {
            continue;
        }
        let json = any_to_json(value);
        semantic.insert(name.to_string(), sem_mark_attrs(name, Some(&json)));
        raw.insert(name.to_string(), json);
    }
    (semantic, raw)
}

/// Attribute names markdown's lossless set covers (block_contract LOSSLESS_MEANS), with the node that
/// carries each: a markdown write cannot state `indent` of a paragraph, only of a list item.
fn markdown_states(tag: &str, attr: &str) -> bool {
    match attr {
        "level" => tag == "heading",
        "listType" | "checked" | "indent" => tag == "listItem",
        "language" => tag == "codeBlock",
        "src" | "alt" | "title" => tag == "image",
        _ => false,
    }
}

/// What html_to_tiptap sets beyond markdown (html_to_tiptap.rs:518-570), on any node it builds.
fn html_extra_states(attr: &str) -> bool {
    matches!(attr, "textAlign" | "collapsed" | "indent")
        || attr.starts_with("data-source-")
        || attr.starts_with("data-pdf-")
}

/// Does a write in `format` state attribute `attr` of node `tag`? Unstated attributes keep their current
/// value on a matched node, and do not take part in its hash. ID_ATTRS: always false (IdPolicy decides).
pub(crate) fn node_attr_stated(format: SourceFormat, tag: &str, attr: &str) -> bool {
    if ID_ATTRS.contains(&attr) {
        return false;
    }
    match format {
        SourceFormat::TiptapJson | SourceFormat::Xml | SourceFormat::Restore => true,
        SourceFormat::Blocks => matches!(attr, "level" | "listType" | "checked" | "language"),
        SourceFormat::Markdown => markdown_states(tag, attr),
        SourceFormat::Html => markdown_states(tag, attr) || html_extra_states(attr),
        SourceFormat::Plain => false,
    }
}

/// Does a write in `format` state mark `mark`?
pub(crate) fn mark_stated(format: SourceFormat, mark: &str) -> bool {
    match format {
        SourceFormat::TiptapJson | SourceFormat::Xml | SourceFormat::Restore => true,
        SourceFormat::Blocks => matches!(
            mark,
            "bold"
                | "italic"
                | "strike"
                | "code"
                | "highlight"
                | "link"
                | "commentMark"
                | "wireMark"
        ),
        // markdown.lossless in block-contract.v1.json
        SourceFormat::Markdown => matches!(mark, "link" | "bold" | "code" | "italic" | "strike"),
        SourceFormat::Html => {
            matches!(mark, "link" | "bold" | "code" | "italic" | "strike")
                || matches!(mark, "underline" | "highlight")
        }
        SourceFormat::Plain => false,
    }
}

/// Can a write in `format` express node `tag` at all? A current node it cannot express is kept when the
/// desired content omits it (Pair::KeptUnstated).
pub(crate) fn node_expressible(format: SourceFormat, tag: &str) -> bool {
    match format {
        // the contract's "(not exported)" nodes, and the placeholder that is never stored
        SourceFormat::Markdown | SourceFormat::Html => !matches!(
            tag,
            "queryBlock" | "calendarEvent" | "citation" | "unsupportedBlock"
        ),
        _ => true,
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64, stable across platforms and toolchains (no new crate: Cargo.lock is --locked).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fnv64(pub(crate) u64);

impl Fnv64 {
    pub(crate) fn new() -> Self {
        Fnv64(FNV_OFFSET)
    }
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
    /// Length-prefixed, so ("ab","c") and ("a","bc") differ.
    pub(crate) fn write_str(&mut self, text: &str) {
        self.write_u64(text.len() as u64);
        self.write(text.as_bytes());
    }
    /// Little-endian, so the hash does not depend on the platform.
    pub(crate) fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }
    /// Canonical: object keys sorted, numbers as f64 bit patterns, a type tag before each value.
    pub(crate) fn write_value(&mut self, value: &Value) {
        match value {
            Value::Null => self.write(&[0]),
            Value::Bool(b) => self.write(&[1, u8::from(*b)]),
            Value::Number(number) => {
                self.write(&[2]);
                let f = number.as_f64().unwrap_or(0.0);
                let f = if f == 0.0 { 0.0 } else { f };
                self.write_u64(f.to_bits());
            }
            Value::String(text) => {
                self.write(&[3]);
                self.write_str(text);
            }
            Value::Array(items) => {
                self.write(&[4]);
                self.write_u64(items.len() as u64);
                for item in items {
                    self.write_value(item);
                }
            }
            Value::Object(map) => {
                self.write(&[5]);
                self.write_u64(map.len() as u64);
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                for (key, item) in entries {
                    self.write_str(key);
                    self.write_value(item);
                }
            }
        }
    }
    pub(crate) fn finish(&self) -> u64 {
        self.0
    }
}

fn attrs_to_object(attrs: &SemAttrs) -> Map<String, Value> {
    attrs
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// A text node: marks in the contract's canonical order, mark attrs omitted when empty.
fn text_node(text: String, marks: &MarkSet) -> Value {
    let mut node = Map::new();
    node.insert("type".into(), Value::String("text".into()));
    node.insert("text".into(), Value::String(text));
    if !marks.is_empty() {
        let mut names: Vec<&String> = marks.keys().collect();
        names.sort_by(|a, b| canonical_mark_cmp(a, b));
        let list = names
            .into_iter()
            .map(|name| {
                let mut mark = Map::new();
                mark.insert("type".into(), Value::String(name.clone()));
                let attrs = &marks[name];
                if !attrs.is_empty() {
                    mark.insert("attrs".into(), Value::Object(attrs_to_object(attrs)));
                }
                Value::Object(mark)
            })
            .collect();
        node.insert("marks".into(), Value::Array(list));
    }
    Value::Object(node)
}

fn normal_children(content: &[Value], with_ids: bool, format: Option<SourceFormat>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pending: Option<(String, MarkSet)> = None;
    for node in content {
        if node.get("type").and_then(Value::as_str) == Some("text") {
            let text = node.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                continue;
            }
            let (mut marks, _) = marks_from_tiptap(node.get("marks"));
            if let Some(format) = format {
                marks.retain(|name, _| mark_stated(format, name));
            }
            if let Some((run, run_marks)) = pending.as_mut() {
                if *run_marks == marks {
                    run.push_str(text);
                    continue;
                }
            }
            if let Some((run, run_marks)) = pending.take() {
                out.push(text_node(run, &run_marks));
            }
            pending = Some((text.to_string(), marks));
            continue;
        }
        if let Some((run, run_marks)) = pending.take() {
            out.push(text_node(run, &run_marks));
        }
        out.push(normal_element(node, with_ids, format));
    }
    if let Some((run, run_marks)) = pending.take() {
        out.push(text_node(run, &run_marks));
    }
    out
}

fn normal_element(node: &Value, with_ids: bool, format: Option<SourceFormat>) -> Value {
    let Some(object) = node.as_object() else {
        return node.clone();
    };
    let tag = object.get("type").and_then(Value::as_str).unwrap_or("");
    let mut out = Map::new();
    out.insert("type".into(), Value::String(tag.to_string()));
    let empty = Map::new();
    let raw = object
        .get("attrs")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut attrs = sem_node_attrs(tag, raw);
    attrs.retain(|key, _| {
        if ID_ATTRS.contains(&key.as_str()) {
            with_ids
        } else {
            format.is_none_or(|format| node_attr_stated(format, tag, key))
        }
    });
    if !attrs.is_empty() {
        out.insert("attrs".into(), Value::Object(attrs_to_object(&attrs)));
    }
    if let Some(Value::Array(content)) = object.get("content") {
        let children = normal_children(content, with_ids, format);
        if !children.is_empty() {
            out.insert("content".into(), Value::Array(children));
        }
    }
    Value::Object(out)
}

fn normal_root(doc: &Value, with_ids: bool, format: Option<SourceFormat>) -> Value {
    if doc.get("type").and_then(Value::as_str) == Some("text") {
        return normal_children(std::slice::from_ref(doc), with_ids, format)
            .into_iter()
            .next()
            .unwrap_or(Value::Null);
    }
    normal_element(doc, with_ids, format)
}

/// The oracle's normal form of a TipTap doc or node (DESIGN §2.15): node and mark attrs as SemAttrs
/// (ids dropped unless `with_ids`), marks in `block_contract::canonical_mark_cmp` order, adjacent text nodes
/// with equal marks merged, empty text nodes and empty `content` arrays dropped.
pub(crate) fn normal_form(doc: &Value, with_ids: bool) -> Value {
    normal_root(doc, with_ids, None)
}

/// The same, restricted to what `format` states: unstated attrs and marks dropped (for markdown oracles).
pub(crate) fn normal_form_stated(doc: &Value, format: SourceFormat, with_ids: bool) -> Value {
    normal_root(doc, with_ids, Some(format))
}

/// Raw marks -> yrs formatting attributes (mark name -> `builder::json_to_any(value)`).
pub(crate) fn raw_marks_to_y(marks: &RawMarks) -> yrs::types::Attrs {
    marks
        .iter()
        .map(|(name, value)| {
            (
                std::sync::Arc::<str>::from(name.as_str()),
                json_to_any(value),
            )
        })
        .collect()
}
