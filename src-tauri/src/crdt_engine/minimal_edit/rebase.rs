//! Three-way rebase of an `expectedRevision` write onto live edits the agent did not see (DESIGN §2.8).

use super::types::{CurrentFragment, DesiredFragment, LiveEdits, WritePolicy};

/// `base`: the record's `tiptapJson` at the expected revision (what `read_document` served). Per top-level
/// block: agent left it as in base -> take current (the person's edit survives); current still equals base
/// -> take desired; both changed -> take current (the person wins, Q2) and name the block in `conflicts`.
pub(crate) fn rebase_on_live(
    base: &DesiredFragment,
    current: &CurrentFragment,
    desired: DesiredFragment,
    policy: &WritePolicy,
) -> (DesiredFragment, LiveEdits) {
    unimplemented!("U8")
}
