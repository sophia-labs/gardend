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
use oxigraph::store::Store;

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
    let _ = (store, graph_id, target);
    unimplemented!("G4")
}

/// The JSON of one row, the contract both doors return (camelCase): `{kind, sourceDocumentId, sourceBlockId?,
/// sourceTitle?, sourceKind?, predicate?, wireId?, viaConnection?}`; absent options are omitted, not null.
pub(crate) fn backlink_json(row: &Backlink) -> serde_json::Value {
    let _ = row;
    unimplemented!("G4")
}

/// The MCP tool `get_backlinks {graphId, documentId? | iri?, blockId?}`. Returns
/// `{graphId, target: {documentId?, iri?, blockId?}, backlinks: [row...], count}`.
pub(crate) fn mcp_local_get_backlinks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let _ = (app, arguments);
    unimplemented!("G4")
}
