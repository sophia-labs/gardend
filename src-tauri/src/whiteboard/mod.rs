//! The whiteboard document kind: a live Excalidraw canvas, drawn on by people in
//! the app and by agents through `whiteboard_create`, `whiteboard_read` and
//! `whiteboard_apply`. The shared Y.Doc format is in `FORMAT.md`.

pub(crate) mod mcp;
pub(crate) mod ops;
pub(crate) mod outline;
pub(crate) mod restore;
#[cfg(test)]
mod restore_tests;
pub(crate) mod scene;
#[cfg(test)]
mod tests;

/// The workspace `documentKind` of a whiteboard.
pub(crate) const WHITEBOARD_KIND: &str = "whiteboard";

/// Documents whose content is not a TipTap fragment, so the TipTap projection is
/// skipped for them: the Mithras Flow board and the whiteboard.
pub(crate) fn is_scene_kind(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some(crate::flow_board::FLOW_BOARD_KIND) | Some(WHITEBOARD_KIND)
    )
}

pub(crate) fn is_whiteboard(kind: Option<&str>) -> bool {
    kind == Some(WHITEBOARD_KIND)
}
