//! The two models the planner compares: the desired fragment (from TipTap JSON) and the current fragment
//! (read from the room's Y.Doc inside the write's transaction). Hashing and inline units live here.

use std::cell::OnceCell;
use std::ops::Range;
use std::sync::Arc;

use serde_json::{Map, Value};
use yrs::types::text::YChange;
use yrs::types::xml::XmlOut;
use yrs::{Any, Out, ReadTxn, Text, Xml, XmlFragment, XmlFragmentRef};

use super::attrs::{
    mark_stated, marks_from_tiptap, marks_from_y, node_attr_stated, sem_node_attrs, Fnv64,
};
use super::types::{
    CurrentChild, CurrentFragment, CurrentNode, CurrentText, DesiredChild, DesiredFragment,
    DesiredNode, MarkSet, Seg, TextRun, Unit, UnitKey, UnitOrigin, WritePolicy,
};

/// Inline node tags: children of a textblock that are Y.XmlElements beside Y.XmlText (projection.rs:57
/// plus the contract's other inline atoms): hardBreak, mathInline, footnote, wikilink, citation, tagChip.
pub(crate) fn is_inline_tag(tag: &str) -> bool {
    matches!(
        tag,
        "hardBreak" | "mathInline" | "footnote" | "wikilink" | "citation" | "tagChip"
    )
}

/// `data-block-id`, else `blockId`; non-empty strings only (block_ops.rs:523-533).
pub(crate) fn block_id(attrs: &Map<String, Value>) -> Option<String> {
    let id = match attrs.get("data-block-id") {
        None | Some(Value::Null) => attrs.get("blockId"),
        id => id,
    };
    match id {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Hashing (DESIGN §2.2). Every hash here is computed under the write's policy: stated attributes and stated
// marks only, block ids never. Current and desired sides run the same code over the same facts.
// ---------------------------------------------------------------------------

fn any_to_json(any: &Any) -> Value {
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

/// The stated semantic attributes of a node, hashed in key order (BTreeMap), ids excluded by
/// `node_attr_stated`.
fn write_stated_attrs(
    fnv: &mut Fnv64,
    tag: &str,
    attrs: &Map<String, Value>,
    policy: &WritePolicy,
) {
    let semantic = sem_node_attrs(tag, attrs);
    let stated: Vec<_> = semantic
        .iter()
        .filter(|(key, _)| node_attr_stated(policy.format, tag, key))
        .collect();
    fnv.write_u64(stated.len() as u64);
    for (key, value) in stated {
        fnv.write_str(key);
        fnv.write_value(value);
    }
}

/// An inline atom's unit key: its tag and stated semantic attributes.
fn atom_key(tag: &str, attrs: &Map<String, Value>, policy: &WritePolicy) -> u64 {
    let mut fnv = Fnv64::new();
    fnv.write(&[b'A']);
    fnv.write_str(tag);
    write_stated_attrs(&mut fnv, tag, attrs, policy);
    fnv.finish()
}

/// The stated marks of a run, hashed as (name, attrs) pairs in name order.
fn marks_hash(marks: &MarkSet, policy: &WritePolicy) -> u64 {
    let mut fnv = Fnv64::new();
    let mut stated = 0u64;
    for (name, attrs) in marks {
        if !mark_stated(policy.format, name) {
            continue;
        }
        stated += 1;
        fnv.write_str(name);
        let object: Map<String, Value> = attrs
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        fnv.write_value(&Value::Object(object));
    }
    fnv.write_u64(stated);
    fnv.finish()
}

/// Streams the units of one inline segment into a hash: a character with its marks hash, or an atom key.
struct InlineHasher {
    fnv: Fnv64,
    units: u64,
}

impl InlineHasher {
    fn new() -> Self {
        let mut fnv = Fnv64::new();
        fnv.write(&[b'I']);
        InlineHasher { fnv, units: 0 }
    }

    fn char(&mut self, c: char, marks_hash: u64) {
        self.fnv.write(&[0]);
        self.fnv.write(&(c as u32).to_le_bytes());
        self.fnv.write_u64(marks_hash);
        self.units += 1;
    }

    fn atom(&mut self, key: u64) {
        self.fnv.write(&[1]);
        self.fnv.write_u64(key);
        self.units += 1;
    }
}

/// Part kinds of a node hash: a block child's hash, or an inline segment's.
const PART_NODE: u8 = 0;
const PART_INLINE: u8 = 1;

fn hash_element(
    tag: &str,
    attrs: &Map<String, Value>,
    parts: &[(u8, u64)],
    policy: &WritePolicy,
) -> u64 {
    let mut fnv = Fnv64::new();
    fnv.write(&[b'N']);
    fnv.write_str(tag);
    write_stated_attrs(&mut fnv, tag, attrs, policy);
    fnv.write_u64(parts.len() as u64);
    for (kind, hash) in parts {
        fnv.write(&[*kind]);
        fnv.write_u64(*hash);
    }
    fnv.finish()
}

fn cur_inline_part(
    children: &[CurrentChild],
    range: Range<usize>,
    policy: &WritePolicy,
) -> Option<u64> {
    let mut hasher = InlineHasher::new();
    for child in &children[range] {
        match child {
            CurrentChild::Text(text) => {
                for run in &text.runs {
                    let marks = marks_hash(&run.marks, policy);
                    for c in run.text.chars() {
                        hasher.char(c, marks);
                    }
                }
            }
            CurrentChild::Element(node) => hasher.atom(atom_key(&node.tag, &node.attrs, policy)),
        }
    }
    // an inline segment with no unit (an empty Y.XmlText) is no content at all
    (hasher.units > 0).then(|| hasher.fnv.finish())
}

fn des_inline_part(
    children: &[DesiredChild],
    range: Range<usize>,
    policy: &WritePolicy,
) -> Option<u64> {
    let mut hasher = InlineHasher::new();
    for child in &children[range] {
        match child {
            DesiredChild::Text(runs) => {
                for run in runs {
                    let marks = marks_hash(&run.marks, policy);
                    for c in run.text.chars() {
                        hasher.char(c, marks);
                    }
                }
            }
            DesiredChild::Element(node) => hasher.atom(atom_key(&node.tag, &node.attrs, policy)),
        }
    }
    (hasher.units > 0).then(|| hasher.fnv.finish())
}

fn cur_parts(children: &[CurrentChild], policy: &WritePolicy) -> Vec<(u8, u64)> {
    let mut parts = Vec::new();
    for seg in segments_cur(children) {
        match seg {
            Seg::Node(index) => {
                if let CurrentChild::Element(node) = &children[index] {
                    parts.push((PART_NODE, node.hash));
                }
            }
            Seg::Inline(range) => {
                if let Some(hash) = cur_inline_part(children, range, policy) {
                    parts.push((PART_INLINE, hash));
                }
            }
        }
    }
    parts
}

fn des_parts(children: &[DesiredChild], policy: &WritePolicy) -> Vec<(u8, u64)> {
    let mut parts = Vec::new();
    for seg in segments_des(children) {
        match seg {
            Seg::Node(index) => {
                if let DesiredChild::Element(node) = &children[index] {
                    parts.push((PART_NODE, node.hash));
                }
            }
            Seg::Inline(range) => {
                if let Some(hash) = des_inline_part(children, range, policy) {
                    parts.push((PART_INLINE, hash));
                }
            }
        }
    }
    parts
}

// ---------------------------------------------------------------------------
// Current model
// ---------------------------------------------------------------------------

/// Read the whole `content` fragment. Never mutates. O(elements + chars).
pub(crate) fn read_fragment<T: ReadTxn>(
    txn: &T,
    fragment: &XmlFragmentRef,
    policy: &WritePolicy,
) -> CurrentFragment {
    CurrentFragment {
        children: read_children(txn, fragment, policy),
    }
}

/// Read one parent's children (the fragment or an element). A nested fragment among the children does not
/// occur in garden documents and is not modelled.
pub(crate) fn read_children<T: ReadTxn, P: XmlFragment>(
    txn: &T,
    parent: &P,
    policy: &WritePolicy,
) -> Vec<CurrentChild> {
    let mut out = Vec::new();
    for child in parent.children(txn) {
        match child {
            XmlOut::Element(element) => {
                out.push(CurrentChild::Element(read_element(txn, element, policy)))
            }
            XmlOut::Text(text) => out.push(CurrentChild::Text(read_text(txn, text))),
            XmlOut::Fragment(_) => {}
        }
    }
    out
}

fn read_element<T: ReadTxn>(
    txn: &T,
    element: yrs::XmlElementRef,
    policy: &WritePolicy,
) -> CurrentNode {
    let tag = element
        .try_tag()
        .map(|tag| tag.to_string())
        .unwrap_or_default();
    // Out::Any as JSON, anything else as null (projection.rs:416-419); sorted so reads are deterministic
    let mut entries: Vec<(String, Value)> = element
        .attributes(txn)
        .map(|(key, value)| {
            let json = match &value {
                Out::Any(any) => any_to_json(any),
                _ => Value::Null,
            };
            (key.to_string(), json)
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let attrs: Map<String, Value> = entries.into_iter().collect();
    let children = read_children(txn, &element, policy);
    let id = block_id(&attrs);
    let hash = hash_element(&tag, &attrs, &cur_parts(&children, policy), policy);
    CurrentNode {
        handle: element,
        tag,
        attrs,
        children,
        id,
        hash,
        image: OnceCell::new(),
    }
}

fn read_text<T: ReadTxn>(txn: &T, text: yrs::XmlTextRef) -> CurrentText {
    let mut runs = Vec::new();
    for diff in text.diff(txn, YChange::identity) {
        let chunk = match diff.insert {
            Out::Any(Any::String(chunk)) => chunk.to_string(),
            // embeds do not occur in garden documents (projection.rs:388-390 skips them too)
            _ => continue,
        };
        if chunk.is_empty() {
            continue;
        }
        let (marks, raw_marks) = marks_from_y(diff.attributes.as_deref());
        runs.push(TextRun {
            text: chunk,
            marks,
            raw_marks,
        });
    }
    let byte_len = text.len(txn);
    CurrentText {
        handle: text,
        runs,
        byte_len,
    }
}

// ---------------------------------------------------------------------------
// Desired model
// ---------------------------------------------------------------------------

/// Desired model of a TipTap `content` array. Consecutive text nodes become one `DesiredChild::Text`;
/// empty text nodes are dropped (builder.rs:42-44 skips them too).
pub(crate) fn desired_from_tiptap_nodes(nodes: &[Value], policy: &WritePolicy) -> DesiredFragment {
    DesiredFragment {
        children: desired_children(nodes, policy),
    }
}

fn desired_children(nodes: &[Value], policy: &WritePolicy) -> Vec<DesiredChild> {
    let mut out = Vec::new();
    let mut runs: Vec<TextRun> = Vec::new();
    for node in nodes {
        if node.get("type").and_then(Value::as_str) == Some("text") {
            let text = node.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                continue;
            }
            let (marks, raw_marks) = marks_from_tiptap(node.get("marks"));
            runs.push(TextRun {
                text: text.to_string(),
                marks,
                raw_marks,
            });
            continue;
        }
        if !runs.is_empty() {
            out.push(DesiredChild::Text(std::mem::take(&mut runs)));
        }
        out.push(DesiredChild::Element(desired_node(node, policy)));
    }
    if !runs.is_empty() {
        out.push(DesiredChild::Text(runs));
    }
    out
}

fn desired_node(node: &Value, policy: &WritePolicy) -> DesiredNode {
    // builder.rs:63 types a node with no type as a paragraph
    let tag = node
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("paragraph")
        .to_string();
    let attrs = node
        .get("attrs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let children = node
        .get("content")
        .and_then(Value::as_array)
        .map(|content| desired_children(content, policy))
        .unwrap_or_default();
    build_desired_node(tag, attrs, children, node.clone(), policy)
}

fn build_desired_node(
    tag: String,
    attrs: Map<String, Value>,
    children: Vec<DesiredChild>,
    json: Value,
    policy: &WritePolicy,
) -> DesiredNode {
    let hash = hash_element(&tag, &attrs, &des_parts(&children, policy), policy);
    let id = block_id(&attrs);
    DesiredNode {
        tag,
        attrs,
        children,
        id,
        hash,
        json,
    }
}

/// One text run as the projection writes it: marks in the contract's canonical order, `attrs` only when the
/// raw value is a non-empty object (projection.rs:359-382).
fn run_json(run: &TextRun) -> Value {
    let mut node = Map::new();
    node.insert("type".into(), Value::String("text".into()));
    node.insert("text".into(), Value::String(run.text.clone()));
    if !run.raw_marks.is_empty() {
        let mut names: Vec<&String> = run.raw_marks.keys().collect();
        names.sort_by(|a, b| crate::crdt_engine::block_contract::canonical_mark_cmp(a, b));
        let marks = names
            .into_iter()
            .map(|name| {
                let mut mark = Map::new();
                mark.insert("type".into(), Value::String(name.clone()));
                if let Value::Object(attrs) = &run.raw_marks[name] {
                    if !attrs.is_empty() {
                        mark.insert("attrs".into(), Value::Object(attrs.clone()));
                    }
                }
                Value::Object(mark)
            })
            .collect();
        node.insert("marks".into(), Value::Array(marks));
    }
    Value::Object(node)
}

/// The TipTap JSON of a current node (tag, raw attrs, children; each text run one text node with its raw
/// marks). Equal to what `projection::ydoc_to_tiptap_json` would give for that node.
pub(crate) fn current_node_json(node: &CurrentNode) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), Value::String(node.tag.clone()));
    if !node.attrs.is_empty() {
        out.insert("attrs".into(), Value::Object(node.attrs.clone()));
    }
    let mut content = Vec::new();
    for child in &node.children {
        match child {
            CurrentChild::Element(element) => content.push(current_node_json(element)),
            CurrentChild::Text(text) => content.extend(text.runs.iter().map(run_json)),
        }
    }
    if !content.is_empty() {
        out.insert("content".into(), Value::Array(content));
    }
    Value::Object(out)
}

/// A current child as a desired child with the same content (used by the rebase to keep a live edit).
pub(crate) fn current_child_to_desired(child: &CurrentChild, policy: &WritePolicy) -> DesiredChild {
    match child {
        CurrentChild::Text(text) => DesiredChild::Text(text.runs.clone()),
        CurrentChild::Element(node) => {
            let children = node
                .children
                .iter()
                .map(|child| current_child_to_desired(child, policy))
                .collect();
            DesiredChild::Element(build_desired_node(
                node.tag.clone(),
                node.attrs.clone(),
                children,
                current_node_json(node),
                policy,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Segments and units
// ---------------------------------------------------------------------------

/// Cut `len` children into segments: each non-inline child is a Node, each maximal run of inline children
/// one Inline range.
fn build_segments(len: usize, inline: impl Fn(usize) -> bool) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut start: Option<usize> = None;
    for index in 0..len {
        if inline(index) {
            start.get_or_insert(index);
        } else {
            if let Some(from) = start.take() {
                segs.push(Seg::Inline(from..index));
            }
            segs.push(Seg::Node(index));
        }
    }
    if let Some(from) = start {
        segs.push(Seg::Inline(from..len));
    }
    segs
}

/// Segment a current children list: each block element is a Node; each maximal run of Text children and
/// inline-tag elements is one Inline segment.
pub(crate) fn segments_cur(children: &[CurrentChild]) -> Vec<Seg> {
    build_segments(children.len(), |index| match &children[index] {
        CurrentChild::Text(_) => true,
        CurrentChild::Element(node) => is_inline_tag(&node.tag),
    })
}

pub(crate) fn segments_des(children: &[DesiredChild]) -> Vec<Seg> {
    build_segments(children.len(), |index| match &children[index] {
        DesiredChild::Text(_) => true,
        DesiredChild::Element(node) => is_inline_tag(&node.tag),
    })
}

fn char_units(
    run: &TextRun,
    marks: &Arc<MarkSet>,
    raw: &Arc<super::types::RawMarks>,
    mut origin: impl FnMut(usize) -> UnitOrigin,
    out: &mut Vec<Unit>,
) {
    for (offset, c) in run.text.char_indices() {
        out.push(Unit {
            key: UnitKey::Char(c),
            marks: Arc::clone(marks),
            raw_marks: Arc::clone(raw),
            origin: origin(offset),
        });
    }
}

/// Units of current children `range` (an Inline segment): every char of every Y.XmlText (CurText origin,
/// byte offsets), every inline atom one CurAtom unit. Atoms are not expanded to images here.
pub(crate) fn units_cur(
    children: &[CurrentChild],
    range: Range<usize>,
    policy: &WritePolicy,
) -> Vec<Unit> {
    let mut out = Vec::new();
    for child in range {
        match &children[child] {
            CurrentChild::Text(text) => {
                // offsets run across the runs of one Y.XmlText
                let mut base = 0usize;
                for run in &text.runs {
                    let marks = Arc::new(run.marks.clone());
                    let raw = Arc::new(run.raw_marks.clone());
                    char_units(
                        run,
                        &marks,
                        &raw,
                        |offset| UnitOrigin::CurText {
                            child,
                            byte: (base + offset) as u32,
                        },
                        &mut out,
                    );
                    base += run.text.len();
                }
            }
            CurrentChild::Element(node) => out.push(Unit {
                key: UnitKey::Atom(atom_key(&node.tag, &node.attrs, policy)),
                marks: Arc::new(MarkSet::new()),
                raw_marks: Arc::new(Default::default()),
                origin: UnitOrigin::CurAtom { child },
            }),
        }
    }
    out
}

pub(crate) fn units_des(
    children: &[DesiredChild],
    range: Range<usize>,
    policy: &WritePolicy,
) -> Vec<Unit> {
    let mut out = Vec::new();
    for child in range {
        match &children[child] {
            DesiredChild::Text(runs) => {
                for (run_index, run) in runs.iter().enumerate() {
                    let marks = Arc::new(run.marks.clone());
                    let raw = Arc::new(run.raw_marks.clone());
                    char_units(
                        run,
                        &marks,
                        &raw,
                        |_| UnitOrigin::DesText {
                            child,
                            run: run_index,
                        },
                        &mut out,
                    );
                }
            }
            DesiredChild::Element(node) => out.push(Unit {
                key: UnitKey::Atom(atom_key(&node.tag, &node.attrs, policy)),
                marks: Arc::new(MarkSet::new()),
                raw_marks: Arc::new(Default::default()),
                origin: UnitOrigin::DesAtom { child },
            }),
        }
    }
    out
}

/// Hash of an inline unit sequence under the policy: keys plus stated marks. Equal for equal content
/// whatever the Y.XmlText segmentation (DESIGN I1 on builder-shaped documents).
pub(crate) fn inline_hash(units: &[Unit], policy: &WritePolicy) -> u64 {
    let mut hasher = InlineHasher::new();
    // units of one run share one Arc: hash its marks once
    let mut last: Option<(Arc<MarkSet>, u64)> = None;
    for unit in units {
        match &unit.key {
            UnitKey::Char(c) => {
                let marks = match &last {
                    Some((seen, hash)) if Arc::ptr_eq(seen, &unit.marks) => *hash,
                    _ => {
                        let hash = marks_hash(&unit.marks, policy);
                        last = Some((Arc::clone(&unit.marks), hash));
                        hash
                    }
                };
                hasher.char(*c, marks);
            }
            UnitKey::Atom(key) => hasher.atom(*key),
        }
    }
    hasher.fnv.finish()
}
