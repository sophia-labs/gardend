//! Forward restore of non-fragment roots (Y.Map/Y.Array/Any trees): comments, the workspace maps, the Flow
//! board's `resource` and `scene`. Changes only what differs from the target.

use yrs::{ArrayRef, MapRef, ReadTxn, TransactionMut};

use super::types::OpCounts;

/// For each named root map: make the current root equal to the target's (an absent target root = empty).
pub(crate) fn restore_map_roots<T: ReadTxn>(
    txn: &mut TransactionMut<'_>,
    roots: &[&str],
    target: &T,
) -> OpCounts {
    unimplemented!("U6")
}

/// Recursive map restore: equal Any values untouched; nested maps and arrays recursed into; differing
/// values replaced (deep copy through `yrs::types::AsPrelim::as_prelim`); keys absent from the target removed.
pub(crate) fn restore_map<T: ReadTxn>(
    txn: &mut TransactionMut<'_>,
    current: &MapRef,
    target_txn: &T,
    target: &MapRef,
) -> OpCounts {
    unimplemented!("U6")
}

/// Array restore by Myers over each element's canonical JSON.
pub(crate) fn restore_array<T: ReadTxn>(
    txn: &mut TransactionMut<'_>,
    current: &ArrayRef,
    target_txn: &T,
    target: &ArrayRef,
) -> OpCounts {
    unimplemented!("U6")
}
