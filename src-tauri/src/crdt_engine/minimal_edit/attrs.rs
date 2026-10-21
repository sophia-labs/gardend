//! Semantic equality, carry rules, hashing and the normal form (DESIGN §2.3).

use serde_json::{Map, Value};

use super::types::{MarkSet, RawMarks, SemAttrs, SourceFormat};

/// Block-id attributes: excluded from every hash and from attribute patches (ids follow IdPolicy).
pub(crate) const ID_ATTRS: &[&str] = &["data-block-id", "blockId"];

/// `None` for JSON null; integral and fractional numbers both as f64 JSON numbers; other values unchanged
/// (objects and arrays recursively).
pub(crate) fn sem_value(value: &Value) -> Option<Value> {
    unimplemented!("U1")
}

/// Raw node attrs -> SemAttrs: the editor schema's defaults for `tag` (DESIGN Appendix B) filled in for
/// absent keys, nulls dropped, numbers normalized. ID_ATTRS are kept (callers exclude them where needed).
pub(crate) fn sem_node_attrs(tag: &str, raw: &Map<String, Value>) -> SemAttrs {
    unimplemented!("U1")
}

/// Raw mark attrs (an object, `{}` or absent) -> SemAttrs with the mark's schema defaults.
pub(crate) fn sem_mark_attrs(mark: &str, raw: Option<&Value>) -> SemAttrs {
    unimplemented!("U1")
}

/// A TipTap `marks` array -> (semantic, raw). Raw values are the mark's `attrs` object, `{}` when absent
/// (the shape builder.rs:80-85 writes).
pub(crate) fn marks_from_tiptap(marks: Option<&Value>) -> (MarkSet, RawMarks) {
    unimplemented!("U1")
}

/// Y formatting attributes of one text chunk -> (semantic, raw). `Any::Map` values become JSON objects;
/// any other value is kept as its JSON (a legacy `true` reads as `true`).
pub(crate) fn marks_from_y(attrs: Option<&yrs::types::Attrs>) -> (MarkSet, RawMarks) {
    unimplemented!("U1")
}

/// Does a write in `format` state attribute `attr` of node `tag`? Unstated attributes keep their current
/// value on a matched node, and do not take part in its hash. ID_ATTRS: always false (IdPolicy decides).
pub(crate) fn node_attr_stated(format: SourceFormat, tag: &str, attr: &str) -> bool {
    unimplemented!("U1")
}

/// Does a write in `format` state mark `mark`?
pub(crate) fn mark_stated(format: SourceFormat, mark: &str) -> bool {
    unimplemented!("U1")
}

/// Can a write in `format` express node `tag` at all? A current node it cannot express is kept when the
/// desired content omits it (Pair::KeptUnstated).
pub(crate) fn node_expressible(format: SourceFormat, tag: &str) -> bool {
    unimplemented!("U1")
}

/// FNV-1a 64, stable across platforms and toolchains (no new crate: Cargo.lock is --locked).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fnv64(pub(crate) u64);

impl Fnv64 {
    pub(crate) fn new() -> Self {
        unimplemented!("U1")
    }
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        unimplemented!("U1")
    }
    /// Length-prefixed, so ("ab","c") and ("a","bc") differ.
    pub(crate) fn write_str(&mut self, text: &str) {
        unimplemented!("U1")
    }
    pub(crate) fn write_u64(&mut self, value: u64) {
        unimplemented!("U1")
    }
    /// Canonical: object keys sorted, numbers as f64 bit patterns, a type tag before each value.
    pub(crate) fn write_value(&mut self, value: &Value) {
        unimplemented!("U1")
    }
    pub(crate) fn finish(&self) -> u64 {
        unimplemented!("U1")
    }
}

/// The oracle's normal form of a TipTap doc or node (DESIGN §2.15): node and mark attrs as SemAttrs
/// (ids dropped unless `with_ids`), marks in `block_contract::canonical_mark_cmp` order, adjacent text nodes
/// with equal marks merged, empty text nodes and empty `content` arrays dropped.
pub(crate) fn normal_form(doc: &Value, with_ids: bool) -> Value {
    unimplemented!("U1")
}

/// The same, restricted to what `format` states: unstated attrs and marks dropped (for markdown oracles).
pub(crate) fn normal_form_stated(doc: &Value, format: SourceFormat, with_ids: bool) -> Value {
    unimplemented!("U1")
}

/// Raw marks -> yrs formatting attributes (mark name -> `builder::json_to_any(value)`).
pub(crate) fn raw_marks_to_y(marks: &RawMarks) -> yrs::types::Attrs {
    unimplemented!("U1")
}
