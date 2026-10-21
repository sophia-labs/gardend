//! Markdown images: what a node becomes after `tiptap_json_to_markdown` and the write path's parser.

use serde_json::Value;

use super::types::{AtomImage, Image, WritePolicy};

/// Export `node` alone (`markdown_export::tiptap_json_to_markdown`), parse it back
/// (`content_parse::parse_write_content_for_operation(md, Some("markdown"), "md-image")`), normalise it
/// (`block_contract::normalise_doc_content`) and hash the result with `policy`.
pub(crate) fn block_image(node: &Value, policy: &WritePolicy) -> Image {
    unimplemented!("U7")
}

/// The image of one inline atom (export a paragraph holding only the atom; read the parsed paragraph's units).
pub(crate) fn inline_atom_image(atom: &Value, policy: &WritePolicy) -> AtomImage {
    unimplemented!("U7")
}
