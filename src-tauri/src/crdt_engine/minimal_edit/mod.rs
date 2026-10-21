//! Minimal CRDT edits: a write states the document it wants, and the engine changes only what differs.
//!
//! `write_document` used to delete the whole `content` fragment and append a new one (document_ops.rs at
//! 524c824, lines 417-421), so every agent write destroyed Yjs item identity, comment marks, the editor's
//! cursor and any concurrent edit. Here a write is planned, then emitted:
//!
//! ```text
//!  desired TipTap JSON ──model::desired_from_tiptap_nodes──▶ DesiredFragment ─┐
//!                                                                              ├─ plan::plan_fragment ─▶ FragmentPlan
//!  room Y.Doc (txn) ────model::read_fragment────────────────▶ CurrentFragment ─┘                            │
//!                                                                                emit::emit_fragment (same txn) ◀┘
//! ```
//!
//! The planner is pure (no yrs mutation) and decides everything; the emitter only executes. Both run inside
//! one `Room::update_doc` transaction, so a live room and a room opened just for the write take the same path.
//! Restore (crdt_engine/forward_restore.rs) reuses the same planner with `WritePolicy::restore()`.
//! Design and invariants: plans/2026-10-21-minimal-edits/DESIGN.md in the hub (§2, §2.14).

pub(crate) mod attrs;
pub(crate) mod emit;
pub(crate) mod md_image;
pub(crate) mod model;
pub(crate) mod plan;
pub(crate) mod rebase;
pub(crate) mod seqdiff;
pub(crate) mod types;
pub(crate) mod ytree;

#[cfg(test)]
mod attrs_tests;
#[cfg(test)]
mod emit_tests;
#[cfg(test)]
mod md_image_tests;
#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod oracle_tests;
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod rebase_tests;
#[cfg(test)]
mod seqdiff_tests;
#[cfg(test)]
mod ytree_tests;

pub(crate) use types::*;

/// Three-way rebase of an `expectedRevision` write onto live edits the agent did not see (DESIGN §2.8).
/// The validation run may set this to `false` if the rebase misbehaves; the two-way path stays correct.
pub(crate) const REBASE_ON_LIVE_EDITS: bool = true;
/// Markdown images: a markdown write that sends back the markdown rendering of a node keeps the node
/// (objectEmbed, wikilink, math, tables, footnotes; DESIGN §2.4). Off = carry rules only.
pub(crate) const MARKDOWN_IMAGES: bool = true;
/// Myers is O((n+m)·d). Past this edit distance an inline run is replaced whole (bounded worst case).
pub(crate) const MAX_INLINE_EDIT_DISTANCE: usize = 4_096;
/// Past this edit distance a gap of children is replaced whole (no positional pairing).
pub(crate) const MAX_CHILDREN_EDIT_DISTANCE: usize = 2_048;
/// Inline runs longer than this many units (both sides together) skip Myers and are replaced whole.
pub(crate) const MAX_INLINE_UNITS: usize = 200_000;
