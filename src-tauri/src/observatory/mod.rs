//! Observatory (`graph_id = "observatory"`) scaffolding — see
//! `plans/observatory-analysis-cell-spec-20260715.md` §3.A / §5 (Lane A) in
//! the `sophia` hub repo for the full design.
//!
//! **A0** owns graph identity (§A.5, `graph_identity`) and a REAL-store,
//! REAL-gate proof harness for the two-writer authority partition (§A.6,
//! `authority_harness`). **A1** owns the `emporium-observatory` vocab pack
//! (`src-tauri/src/emporium/vocabs/emporium-observatory.golden.json`,
//! publish-only, registered in `emporium/vocabs.rs`). **A2** (`mapping`) owns
//! the PURE JSON-MO → RDF triple mapping — no `Store`, no reconcile, no I/O.
//! **A3** (`apply`) owns the direct-on-store apply that actually WRITES
//! `mapping`'s triples into `:projection:obs:*` — `catch_up(store, bundle,
//! raw_snapshot)`, built on `emporium::reconcile`'s survey→diff→apply seam
//! (SHACL-gated, never a blind `CLEAR`/`DROP`), plus the durable-cursor
//! freshness triple (§A.3/§A.4). `rdf_authority.rs` is READ by
//! `authority_harness` (it calls the real gate functions) but never EDITED —
//! by A0, A3, or any other slice.
//!
//! `pub` (not the plain `mod` a purely-internal module would use) because
//! `src-tauri/tests/observatory_authority.rs` — an external integration-test
//! crate, like `examples/gardend.rs` — needs a real path to these functions.
//! This mirrors the existing `pub mod headless { pub use crate::...; }`
//! bridge `lib.rs` already builds for `examples/gardend.rs`: a curated public
//! surface over otherwise crate-private internals, not a general publicity
//! grant.
//!
//! **A4** (`boot`) owns the garden-side half of the boot-hook: the
//! graph_id + EFS-discoverable-activation gate (RE-SCOPE r1 — no pod env
//! var; see `boot`'s own module doc), the bounded wait for the standalone
//! projector's EFS bundle-dir file contract, and the call into
//! `apply::catch_up` — wired from `examples/gardend.rs`. See `boot`'s
//! module doc for the platform-next-scoped half this deliberately does NOT
//! own, and for the immutable-generation protocol and hot-cell periodic
//! re-check it adds on top of A4's original design.

pub mod apply;
pub mod authority_harness;
pub mod boot;
pub mod graph_identity;
pub mod mapping;
