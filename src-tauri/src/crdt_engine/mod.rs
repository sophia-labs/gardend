//! In-process CRDT engine (platform-next Phase 2).
//!
//! Target architecture (decided 2026-06-11): the Rust core is the Y.Doc
//! authority everywhere — it hosts y-websocket rooms, applies API/MCP
//! operations as in-process transactions, and owns all materialization.
//! The desktop webview and web frontend are sync clients.
//!
//! Ports of the frontend reference implementations:
//! - `projection`: frontend/src/native/document-schema.ts +
//!   frontend/src/crdt/document-roundtrip.ts (Y.Doc/TipTap JSON → tree,
//!   blocks, plain text). Verified against captured fixtures in
//!   tests/fixtures/yrs_wire_compat/.

#[doc(hidden)]
pub mod block_ops;
pub mod builder;
pub(crate) mod content_parity;
pub(crate) mod content_parse;
pub(crate) use content_parse::block_contract;
#[cfg(test)]
mod block_contract_tests;
pub(crate) mod create_once;
#[doc(hidden)]
pub mod document_ops;
pub(crate) mod executor;
pub(crate) mod flow_ops;
pub(crate) mod flush_ops;
pub(crate) mod forward_restore;
#[cfg(test)]
mod forward_restore_tests;
#[doc(hidden)]
pub mod import_archive_ops;
pub(crate) mod import_vault_ops;
pub(crate) mod markdown_export;
pub(crate) mod minimal_edit;
pub(crate) mod persistence_coordinator;
pub mod projection;
pub mod rooms;
pub(crate) mod upload_ingest_ops;
#[doc(hidden)]
pub mod web_clip_ops;
#[doc(hidden)]
pub mod workspace_ops;

#[cfg(test)]
mod comment_anchoring_tests;
#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod document_write_tests;
#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod flow_board_flush_tests;
#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod persistence_tests;
#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
pub(crate) mod workspace_ops_poison_tests;
