//! Sequence alignment primitives: Myers' O((n+m)·d) diff, LIS anchoring, common affixes. Pure, no yrs.

/// One edit of a Myers script. Indices are into the input slices. A script covers both inputs exactly,
/// in order, with adjacent ops of the same kind merged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    Equal { a: usize, b: usize, len: usize },
    Delete { a: usize, len: usize },
    Insert { b: usize, len: usize },
}

/// Shortest edit script from `a` to `b` under `eq`, or `None` when the edit distance exceeds `max_d`.
/// Deterministic: on ties a Delete is emitted before an Insert.
pub(crate) fn myers_by<A, B, F>(a: &[A], b: &[B], eq: F, max_d: usize) -> Option<Vec<Edit>>
where
    F: Fn(&A, &B) -> bool,
{
    unimplemented!("U3")
}

/// Longest common prefix and suffix lengths (suffix never overlaps the prefix).
pub(crate) fn common_affixes<A, B, F>(a: &[A], b: &[B], eq: F) -> (usize, usize)
where
    F: Fn(&A, &B) -> bool,
{
    unimplemented!("U3")
}

/// The longest chain of `pairs` strictly increasing in both coordinates. Input order is irrelevant; output
/// is sorted by `.0`. Ties broken by the smaller `.1`, then by input order (deterministic). O(k log k).
pub(crate) fn longest_increasing_pairs(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    unimplemented!("U3")
}
