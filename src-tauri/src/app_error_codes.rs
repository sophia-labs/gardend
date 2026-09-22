//! The closed machine-readable fault taxonomy. A code is a promise: a client
//! may branch on it. Adding one is a contract change; renaming one is a
//! break.
//!
//! Spec: plans/mo-object-face-integration-spec-20260730/04-garden-gateway-contract.md
//! §4.2.1, master §2.15.

// ── Law VI · lifetime and fence ──
pub(crate) const STALE_GRAPH_INCARNATION: &str = "stale_graph_incarnation";
pub(crate) const STALE_DOCUMENT_INCARNATION: &str = "stale_document_incarnation";
pub(crate) const DOCUMENT_EXISTS: &str = "document_exists";
pub(crate) const DOCUMENT_TOMBSTONED: &str = "document_tombstoned";
pub(crate) const SOURCE_BODY_UNAVAILABLE: &str = "source_body_unavailable";
pub(crate) const RECREATE_BOUNDARY_MISMATCH: &str = "recreate_boundary_mismatch";

// ── Law IV · contested current state ──
pub(crate) const STALE_SYNC_CONFLICT: &str = "stale_sync_conflict";
pub(crate) const CAUSAL_CYCLE: &str = "causal_cycle";

// ── identity and idempotence ──
pub(crate) const OPERATION_ID_REUSED: &str = "operation_id_reused";
pub(crate) const EVENT_IDENTITY_REUSED: &str = "event_identity_reused";

// ── durable ledger integrity ──
pub(crate) const LEDGER_INTEGRITY: &str = "ledger_integrity";
pub(crate) const SOURCE_BUNDLE_TOO_LARGE: &str = "source_bundle_too_large";

// ── emporium object layer ──
// This codes NotFound only for genuine absence of a well-formed object
// (`ObjectError::Absent`). The vocab/class-misconfiguration `NotFound`
// variants stay uncoded — they mean "this class is misconfigured", not
// "this object is absent", and coding them alike would collapse a real
// setup error into a quiet empty card. See emporium/objects.rs.
pub(crate) const OBJECT_NOT_FOUND: &str = "object_not_found";

// ── hosted tenancy (gateway-originated) ──
pub(crate) const GRAPH_EXISTS: &str = "graph_exists";
pub(crate) const GRAPH_LIFECYCLE_IDENTITY_MISMATCH: &str = "graph_lifecycle_identity_mismatch";
pub(crate) const GRAPH_INCARNATION_UNCONFIRMABLE: &str = "graph_incarnation_unconfirmable";

/// Every code, in taxonomy order. The parity test asserts this is exactly
/// the set the client's `SOURCE_FAULT_CODES` union contains (minus the
/// gateway-only rate/quota codes this crate never originates).
pub(crate) const ALL: &[&str] = &[
    STALE_GRAPH_INCARNATION,
    STALE_DOCUMENT_INCARNATION,
    DOCUMENT_EXISTS,
    DOCUMENT_TOMBSTONED,
    SOURCE_BODY_UNAVAILABLE,
    RECREATE_BOUNDARY_MISMATCH,
    STALE_SYNC_CONFLICT,
    CAUSAL_CYCLE,
    OPERATION_ID_REUSED,
    EVENT_IDENTITY_REUSED,
    LEDGER_INTEGRITY,
    SOURCE_BUNDLE_TOO_LARGE,
    OBJECT_NOT_FOUND,
    GRAPH_EXISTS,
    GRAPH_LIFECYCLE_IDENTITY_MISMATCH,
    GRAPH_INCARNATION_UNCONFIRMABLE,
];
