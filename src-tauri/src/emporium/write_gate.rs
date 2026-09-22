//! Per-graph write gate — serializes MUTATING emporium ingests.
//!
//! The spine's survey → plan → apply cycle is a read-modify-write with no
//! transaction spanning it: two concurrent ingests that touch the same logical
//! entity can both survey the same baseline and both apply — forking
//! supersession heads (memory), duplicating single-valued predicates (workflow
//! update-mode), or racing a sibling's just-accepted record (chamber / generic
//! class reconcile). One cell owns one graph, so a per-graph async mutex held
//! across the WHOLE cycle restores the same-current-store premise (`hsame` in
//! the Lean models) that the reconcile algebra's EQUIVALENCE theorem assumes.
//!
//! Scope: the gate is taken by the MUTATING spine entry points only —
//! the ingest route (`dry_run=false`), the memory ingest funnel (which also
//! holds it across the legacy-queue append, keeping "queue and projection
//! never diverge" true under concurrency), and chamber's propose (which holds
//! it across its prior-record resolve, the survey of ITS read-modify-write).
//! Read-only paths (dry-run plans, serving, SPARQL SELECT) never take it.
//!
//! This is deliberately the whitepaper's "stop the bleeding" move: coarse,
//! ~zero doctrine cost, correct for the one-cell-per-graph topology. The
//! long-term convergence story for born-RDF is the event-log projection; this
//! gate is what makes the interim honest.
//!
//! ## Durability: this gate is ALSO the Emporium dirty-marking choke point
//!
//! Emporium's real writes (the memory applier's direct-on-store SPARQL,
//! simple-projection's `reconcile.rs::apply_diff`, the applied-plan journal,
//! the memory event log, chamber's propose, generic-class objects) are
//! plain-`&Store`/plain-file writers that never call
//! `cell_durability::mark_rdf_store_written` or acquire a
//! `crdt_engine::persistence_coordinator::GraphPersistenceLease` — so without
//! this, the durable flush's dirty tracking could never see them at all
//! (a real, acknowledged Emporium write silently invisible to `flush()`,
//! found by a durability audit of the dirty-driven flush scheduler).
//!
//! Rather than instrument each of those sites individually (more places to
//! miss on the next new writer), this gate's own documented invariant —
//! "taken by the MUTATING spine entry points only" — makes it the natural
//! SINGLE choke point: every acquisition already means "a real mutation may
//! be about to happen here," which is exactly the coarse, fail-safe-toward-
//! dirty premise `GraphPersistenceLease::drop` uses. [`WriteGateGuard`]'s
//! `Drop` therefore also calls
//! `mark_graph_rdf_stores_written_with_context(graph_id, ...)`,
//! unconditionally, mirroring `GraphPersistenceLease`'s default (there is no
//! read-only opt-out here — nothing currently proves any acquisition of this
//! gate read-only, and none should be added without the same by-inspection
//! rigor `GraphPersistenceLease::declare_rdf_read_only` requires).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

static WRITE_GATES: OnceLock<Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();

/// RAII guard for [`acquire_write_gate`]. On `Drop`, marks the graph's
/// Oxigraph stores dirty via the same coarse, conservative fallback
/// `GraphPersistenceLease::drop` uses — see the module doc's "Durability"
/// section for why this exists. Marking on `Drop` (not on acquire) matters:
/// it means the epoch bump happens only after the critical section's writes
/// have actually landed, so a concurrent flush that started before this
/// guard was acquired correctly does NOT get to claim this write — it stays
/// dirty for the next flush, matching `GraphPersistenceLease`'s discipline
/// exactly.
pub(crate) struct WriteGateGuard {
    _guard: OwnedMutexGuard<()>,
    graph_id: String,
}

impl Drop for WriteGateGuard {
    fn drop(&mut self) {
        crate::cell_durability::mark_graph_rdf_stores_written_with_context(
            &self.graph_id,
            crate::cell_durability_trace::DirtyReason::EmporiumGateFallback,
            crate::cell_durability_trace::DirtyOrigin::Emporium,
            None,
        );
    }
}

/// Acquire the write gate for `graph_id`, waiting if another mutating ingest
/// holds it. Hold the returned guard across survey → plan → apply and any
/// coupled post-apply writes (e.g. the memory queue append). Guards for
/// DIFFERENT graphs never contend.
pub(crate) async fn acquire_write_gate(graph_id: &str) -> WriteGateGuard {
    let gate = {
        // The outer lock guards only the map; a poisoned map is still a valid
        // map (entries are insert-only), so recover rather than propagate.
        let mut gates = WRITE_GATES
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            gates
                .entry(graph_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    };
    WriteGateGuard {
        _guard: gate.lock_owned().await,
        graph_id: graph_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Two writers on the SAME graph: critical sections must not overlap.
    /// Each task records (enter, exit) around a yield point; strict nesting
    /// (0 concurrent at every enter) proves mutual exclusion.
    #[test]
    fn same_graph_writers_are_serialized() {
        crate::app_runtime::async_runtime::block_on(async {
            let in_section = Arc::new(AtomicUsize::new(0));
            let max_seen = Arc::new(AtomicUsize::new(0));
            let run = |graph: &'static str| {
                let in_section = Arc::clone(&in_section);
                let max_seen = Arc::clone(&max_seen);
                async move {
                    let _gate = acquire_write_gate(graph).await;
                    let now = in_section.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    // Cross a real await point while holding the gate so the
                    // other task gets a chance to (wrongly) enter.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    in_section.fetch_sub(1, Ordering::SeqCst);
                }
            };
            tokio::join!(run("gate-lab"), run("gate-lab"), run("gate-lab"));
            assert_eq!(
                max_seen.load(Ordering::SeqCst),
                1,
                "same-graph critical sections overlapped"
            );
        });
    }

    /// Writers on DIFFERENT graphs must not contend: with one gate held, the
    /// other graph's gate is still immediately acquirable.
    #[test]
    fn different_graphs_do_not_contend() {
        crate::app_runtime::async_runtime::block_on(async {
            let _held = acquire_write_gate("gate-a").await;
            let other =
                tokio::time::timeout(Duration::from_millis(200), acquire_write_gate("gate-b"))
                    .await;
            assert!(
                other.is_ok(),
                "acquiring a different graph's gate blocked behind gate-a"
            );
        });
    }

    /// Re-acquiring the same graph's gate after release works (no leak of a
    /// locked guard through the map).
    #[test]
    fn gate_is_reusable_after_release() {
        crate::app_runtime::async_runtime::block_on(async {
            {
                let _g = acquire_write_gate("gate-reuse").await;
            }
            let again =
                tokio::time::timeout(Duration::from_millis(200), acquire_write_gate("gate-reuse"))
                    .await;
            assert!(again.is_ok(), "gate not released on guard drop");
        });
    }
}
