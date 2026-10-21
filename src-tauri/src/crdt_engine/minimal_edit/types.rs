//! Shared types of the minimal-edit engine. Every unit codes against these; none may change them.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value};
use yrs::{XmlElementRef, XmlTextRef};

/// Which carrier stated the desired content. It decides which attributes, marks and nodes the write
/// *states* (and so may change or remove) and which it leaves as they are (DESIGN §2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceFormat {
    /// `tiptapJson` payloads: the caller owns every attribute and mark of every node it sends.
    TiptapJson,
    /// The simplified `blocks` array (`block_to_tiptap_node`, document_ops.rs:809).
    Blocks,
    /// A `content` string parsed as markdown.
    Markdown,
    /// A `content` string parsed as HTML (`html_to_tiptap`).
    Html,
    /// A `content` string parsed as TipTap XML: carries every attribute, like TiptapJson.
    Xml,
    /// A `content` string parsed as plain text.
    Plain,
    /// Time-travel restore: the target is a stored Y.Doc; everything is stated.
    Restore,
}

impl SourceFormat {
    /// The parser's `source_format` string (`ParsedWriteContent::source_format`, content_parse.rs:74-75).
    pub(crate) fn from_parsed(source_format: &str) -> Self {
        match source_format {
            "markdown" => SourceFormat::Markdown,
            "html" => SourceFormat::Html,
            "xml" => SourceFormat::Xml,
            _ => SourceFormat::Plain,
        }
    }

    /// Formats that state every attribute, mark and node.
    pub(crate) fn states_everything(self) -> bool {
        matches!(
            self,
            SourceFormat::TiptapJson | SourceFormat::Xml | SourceFormat::Restore
        )
    }
}

/// How desired block ids relate to current ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IdPolicy {
    /// A desired id that is present is the caller's: a matched node whose id differs gets the desired id.
    /// A desired node with no id keeps the current node's id (a write never strips an id).
    Bound,
    /// Ids in `minted` were minted by this operation (the content parser, content_parse.rs:1503-1534) and
    /// mean nothing: a matched node keeps the current id. Ids not in `minted` are treated as Bound.
    Adopt { minted: BTreeSet<String> },
}

#[derive(Clone, Debug)]
pub(crate) struct WritePolicy {
    pub(crate) format: SourceFormat,
    pub(crate) ids: IdPolicy,
}

impl WritePolicy {
    pub(crate) fn restore() -> Self {
        WritePolicy {
            format: SourceFormat::Restore,
            ids: IdPolicy::Bound,
        }
    }

    /// True when `id` was minted by this operation (Adopt) and so carries no identity.
    pub(crate) fn id_is_minted(&self, id: &str) -> bool {
        match &self.ids {
            IdPolicy::Bound => false,
            IdPolicy::Adopt { minted } => minted.contains(id),
        }
    }
}

/// Semantic attributes: schema defaults filled in, nulls dropped, every number as an f64 JSON number,
/// keys sorted. Two raw maps are "the same to the editor" iff their SemAttrs are equal (DESIGN §2.3).
pub(crate) type SemAttrs = BTreeMap<String, Value>;

/// The marks of one character: mark name (the Y formatting key) -> semantic mark attrs (empty for none).
pub(crate) type MarkSet = BTreeMap<String, SemAttrs>;

/// Raw marks as they must be written to Y: mark name -> raw attrs JSON object (`{}` for none).
/// This is exactly what `builder::marks_to_attrs` (builder.rs:69-87) and y-tiptap's `marksToAttributes` write.
pub(crate) type RawMarks = BTreeMap<String, Value>;

/// One run of text with uniform marks.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TextRun {
    pub(crate) text: String,
    pub(crate) marks: MarkSet,
    pub(crate) raw_marks: RawMarks,
}

/// A desired element (from TipTap JSON).
#[derive(Clone, Debug)]
pub(crate) struct DesiredNode {
    pub(crate) tag: String,
    /// Raw attrs exactly as the JSON states them (nulls kept; the emitter skips nulls like builder.rs:57-62).
    pub(crate) attrs: Map<String, Value>,
    pub(crate) children: Vec<DesiredChild>,
    /// `data-block-id`, else `blockId`, non-empty strings only (block_ops.rs:523-533).
    pub(crate) id: Option<String>,
    /// Hash of this subtree under the write's policy, block ids excluded (model.rs defines it).
    pub(crate) hash: u64,
    /// The node's own JSON, inserted verbatim (through `builder::insert_node_at`) when the node is new.
    pub(crate) json: Value,
}

#[derive(Clone, Debug)]
pub(crate) enum DesiredChild {
    Element(DesiredNode),
    /// A maximal run of consecutive TipTap text nodes (y-tiptap's `normalizePNodeContent` grouping).
    Text(Vec<TextRun>),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DesiredFragment {
    pub(crate) children: Vec<DesiredChild>,
}

/// A current element, read from the room's Y.Doc inside the write's transaction.
#[derive(Clone, Debug)]
pub(crate) struct CurrentNode {
    /// Valid only inside the transaction that read it. The emitter uses it for Keep/Patch nodes, which
    /// are never deleted by the plan, so the branch is alive when used.
    pub(crate) handle: XmlElementRef,
    pub(crate) tag: String,
    /// Raw attrs read from Y (`Out::Any` values as JSON; any other `Out` reads as null, as projection.rs:416-419).
    pub(crate) attrs: Map<String, Value>,
    pub(crate) children: Vec<CurrentChild>,
    pub(crate) id: Option<String>,
    pub(crate) hash: u64,
    /// Markdown image, computed lazily by the planner (only for nodes it cannot otherwise match).
    pub(crate) image: OnceCell<Image>,
}

#[derive(Clone, Debug)]
pub(crate) enum CurrentChild {
    Element(CurrentNode),
    /// One Y.XmlText child. Several may be adjacent (builder.rs writes one per TipTap text node).
    Text(CurrentText),
}

#[derive(Clone, Debug)]
pub(crate) struct CurrentText {
    pub(crate) handle: XmlTextRef,
    /// Its formatting chunks in order (`Text::diff`, as block_ops.rs:799-807 reads them).
    pub(crate) runs: Vec<TextRun>,
    /// Length in the Doc's offset unit. gardend Docs use `OffsetKind::Bytes` (yrs default, doc.rs:560/573),
    /// so this is UTF-8 bytes.
    pub(crate) byte_len: u32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CurrentFragment {
    pub(crate) children: Vec<CurrentChild>,
}

/// How a current node looks after a markdown round trip (DESIGN §2.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Image {
    /// Markdown carries the node losslessly: its own hash is its image.
    Same,
    /// export→parse yields `count` desired nodes whose combined hash is `hash`.
    Nodes { hash: u64, count: usize },
    /// Markdown cannot express the node at all ("(not exported)" in block-contract.v1.json): a markdown
    /// write that omits it leaves it in place.
    Unexpressible,
}

/// How a current inline atom looks after a markdown round trip.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AtomImage {
    Same,
    /// The atom reads back as these inline units (e.g. mathInline -> "$src$" characters).
    Units(Vec<UnitKey>),
    Unexpressible,
}

/// A segment of a children list: one block-level element, or a maximal run of inline children
/// (Y.XmlText children and inline-atom elements such as hardBreak, mathInline, wikilink).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    Node(usize),
    Inline(Range<usize>),
}

/// The comparison key of one inline unit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UnitKey {
    Char(char),
    /// An inline atom: hash of its tag and stated semantic attrs.
    Atom(u64),
}

/// Where a unit came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnitOrigin {
    /// A character of current child `child` (a Y.XmlText) at UTF-8 byte offset `byte` within it.
    CurText { child: usize, byte: u32 },
    /// A current inline atom element.
    CurAtom { child: usize },
    /// One unit of a current atom's markdown image (the planner expands atoms under Markdown).
    CurAtomImage { child: usize },
    /// A character of desired child `child`, run `run`.
    DesText { child: usize, run: usize },
    /// A desired inline atom element.
    DesAtom { child: usize },
}

#[derive(Clone, Debug)]
pub(crate) struct Unit {
    pub(crate) key: UnitKey,
    /// Semantic marks (empty for atoms).
    pub(crate) marks: Arc<MarkSet>,
    /// Raw marks (empty for atoms): written verbatim on insert, inherited for unstated marks.
    pub(crate) raw_marks: Arc<RawMarks>,
    pub(crate) origin: UnitOrigin,
}

/// One instruction of a plan for one parent's children. Steps run left to right with a live child cursor:
/// Keep/Patch/Text act on the child at the cursor and advance it; Remove(n) removes n children at the cursor;
/// InsertNode/InsertText insert at the cursor and advance it. Every current child is consumed exactly once.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    Keep,
    Patch(NodePatch),
    /// The child at the cursor is a Y.XmlText; apply this delta to it.
    Text(Vec<TextOp>),
    Remove(u32),
    /// Insert this element JSON (builder shape) at the cursor.
    InsertNode(Value),
    /// Insert a new Y.XmlText holding these runs at the cursor.
    InsertText(Vec<TextRun>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NodePatch {
    /// Attributes to set, raw JSON values (never null).
    pub(crate) set: Vec<(String, Value)>,
    pub(crate) remove: Vec<String>,
    /// `None`: children untouched. `Some(steps)`: a full step stream over the node's children.
    pub(crate) children: Option<Vec<Step>>,
}

/// One op of a Y.XmlText delta. Lengths are UTF-8 bytes and always fall on char boundaries.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TextOp {
    Retain(u32),
    /// Retain `bytes` and change formatting: `set` mark -> raw attrs object, `remove` mark names (to null).
    Format {
        bytes: u32,
        set: Vec<(String, Value)>,
        remove: Vec<String>,
    },
    Delete(u32),
    /// Insert `text` carrying exactly `marks` (yrs unsets every other attribute, text.rs:703-724).
    Insert {
        text: String,
        marks: RawMarks,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlanSummary {
    pub(crate) kept: u32,
    pub(crate) patched: u32,
    pub(crate) inserted: u32,
    pub(crate) removed: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FragmentPlan {
    pub(crate) steps: Vec<Step>,
    pub(crate) summary: PlanSummary,
}

impl FragmentPlan {
    /// Every step is Keep: emitting it performs zero Yjs operations.
    pub(crate) fn is_noop(&self) -> bool {
        self.steps.iter().all(|step| matches!(step, Step::Keep))
    }
}

/// An alignment pair between two segment lists (a: current or base, b: desired).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pair {
    /// Same under the policy: keep a, zero ops.
    Equal(usize, usize),
    /// a's markdown image equals b..b+count: keep a, consume `count` b segments.
    EqualImage { a: usize, b: usize, count: usize },
    /// Same tag, different content: patch a toward b.
    Modified(usize, usize),
    /// a has no counterpart.
    Removed(usize),
    /// b has no counterpart.
    Added(usize),
    /// a is unexpressible in the write's format and absent from b: keep a.
    KeptUnstated(usize),
}

/// What one side of an alignment exposes to `plan::align`.
pub(crate) trait Side {
    fn len(&self) -> usize;
    /// Element tag of a Node segment, `"#inline"` for an Inline segment.
    fn tag(&self, seg: usize) -> &str;
    fn id(&self, seg: usize) -> Option<&str>;
    /// Hash under the policy, block ids excluded.
    fn hash(&self, seg: usize) -> u64;
    /// Markdown image (current side, Node segments, Markdown format only); `None` elsewhere.
    fn image(&self, seg: usize) -> Option<&Image>;
    /// True when the segment is a node the write's format cannot express (current side only).
    fn unexpressible(&self, seg: usize) -> bool;
}

/// Counted Yjs operations of one emission (DESIGN §2.14, I1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpCounts {
    pub(crate) nodes_inserted: u32,
    pub(crate) nodes_removed: u32,
    pub(crate) attrs_set: u32,
    pub(crate) attrs_removed: u32,
    pub(crate) text_bytes_inserted: u32,
    pub(crate) text_bytes_deleted: u32,
    pub(crate) format_ranges: u32,
    pub(crate) map_keys_set: u32,
    pub(crate) map_keys_removed: u32,
    pub(crate) array_items_inserted: u32,
    pub(crate) array_items_removed: u32,
}

impl OpCounts {
    pub(crate) fn is_zero(&self) -> bool {
        *self == OpCounts::default()
    }

    pub(crate) fn add(&mut self, other: &OpCounts) {
        self.nodes_inserted += other.nodes_inserted;
        self.nodes_removed += other.nodes_removed;
        self.attrs_set += other.attrs_set;
        self.attrs_removed += other.attrs_removed;
        self.text_bytes_inserted += other.text_bytes_inserted;
        self.text_bytes_deleted += other.text_bytes_deleted;
        self.format_ranges += other.format_ranges;
        self.map_keys_set += other.map_keys_set;
        self.map_keys_removed += other.map_keys_removed;
        self.array_items_inserted += other.array_items_inserted;
        self.array_items_removed += other.array_items_removed;
    }
}

/// Emission result. `client_id` and the clock range are the Phase D attribution seam: the items this write
/// created are exactly `(client_id, clock_before..clock_after)`.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EmitStats {
    pub(crate) counts: OpCounts,
    pub(crate) client_id: u64,
    pub(crate) clock_before: u32,
    pub(crate) clock_after: u32,
}

/// Top-level blocks where a three-way rebase kept a person's live edit (DESIGN §2.8). `conflicts`: blocks both the
/// agent and the person changed since the base; the person's version stays and the agent's change to that block is
/// not applied, so the agent re-reads and redoes it (Vera 2026-10-21, Q2: the person wins).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LiveEdits {
    pub(crate) kept: Vec<String>,
    pub(crate) conflicts: Vec<String>,
}
