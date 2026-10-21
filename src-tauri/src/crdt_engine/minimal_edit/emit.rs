//! The emitter: executes a FragmentPlan as yrs operations inside the caller's transaction. Decides nothing.

use yrs::{TransactionMut, XmlFragmentRef};

use super::types::{CurrentFragment, EmitStats, FragmentPlan};

/// Check a plan against the model it was made from before any mutation: step streams consume every current
/// child exactly once, Text steps address Y.XmlText children, byte counts fit and land on char boundaries.
pub(crate) fn validate_plan(current: &CurrentFragment, plan: &FragmentPlan) -> Result<(), String> {
    unimplemented!("U5")
}

/// Validate, then execute. A plan that is all Keep performs no yrs call at all.
pub(crate) fn emit_fragment(
    txn: &mut TransactionMut<'_>,
    fragment: &XmlFragmentRef,
    current: &CurrentFragment,
    plan: &FragmentPlan,
) -> Result<EmitStats, String> {
    unimplemented!("U5")
}
