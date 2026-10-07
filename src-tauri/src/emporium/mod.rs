//! Emporium — re-homed into gardend (Phase 1).
//!
//! Emporium turns a Mnemosyne graph into a registry + provenance ledger for
//! multi-agent workflows. The gen-2 port re-homes it from the Python platform
//! into the gardend cell as an in-process Rust engine; the gateway stays a
//! blind proxy. Phase 1 ships only the **vocabulary registry**: the canonical
//! `wf:` contract, served as sha-pinned, cacheable bytes over HTTP and MCP.
//!
//! Later phases now landing here: the survey ([`survey`]), the pure planner
//! ([`planner`]), the read-only ingest spine ([`spine`]) that sequences the
//! reads and runs the planner, the CRDT applier ([`applier`]) — the loud-halt
//! write path that maps each plan verb to gardend's in-process CRDT surface and
//! folds RDF + emp: provenance into the user:rdf graph (read==write) — and the
//! pure assertion suite ([`asserts`]) that verifies a workflow's anatomy against
//! the contract after a post-apply re-survey.

pub(crate) mod applied_journal;
pub(crate) mod applier;
pub(crate) mod asserts;
pub(crate) mod chamber;
pub(crate) mod chamber_ontology;
pub(crate) mod class_dispatch;
pub(crate) mod content;
pub(crate) mod folder_views;
pub(crate) mod contract;
#[cfg(test)]
mod domain_kit_tests;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod flow_projection_tests;
pub(crate) mod ingest_routes;
#[cfg(all(test, feature = "headless"))]
mod lex_scotus_core_tests;
#[cfg(test)]
mod machine_core_tests;
pub(crate) mod mcp;
pub(crate) mod memory_applier;
pub(crate) mod memory_events;
pub(crate) mod mint;
pub(crate) mod object_query;
pub(crate) mod objects;
pub(crate) mod openapi_emit;
pub(crate) mod planner;
pub(crate) mod query_emit;
pub(crate) mod query_engine;
pub(crate) mod reconcile;
pub(crate) mod schemas;
pub(crate) mod shacl_emit;
pub(crate) mod shacl_sparql;
pub(crate) mod shacl_validator;
#[cfg(test)]
mod site_projection_tests;
pub(crate) mod spine;
#[cfg(all(test, feature = "headless"))]
mod state_trace_tests;
pub(crate) mod subject_rule;
pub(crate) mod survey;
pub(crate) mod sweep;
pub(crate) mod terms;
pub(crate) mod violation_ledger;
pub(crate) mod vocab_routes;
pub(crate) mod vocabs;
pub(crate) mod write;
pub(crate) mod write_gate;

pub(crate) use mcp::mcp_local_emporium_vocab;
pub(crate) use vocab_routes::loopback_emporium_router;
