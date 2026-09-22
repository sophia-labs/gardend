//! End-to-end contract proof for the Domain Kit reserved projection packs.
//!
//! These tests traverse the same generic planner and SHACL validator used by
//! `/emporium/ingest/{graph}`. They intentionally do not call a direct RDF
//! writer: the whole point is to prove the durable host's authority seam.

use crate::emporium::{
    contract::get_vocabulary,
    planner::plan_generic_compute,
    schemas::GenericRecordIn,
    shacl_validator::{compile_check_contract, validate_desired},
};
use serde_json::{json, Value as Json};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn records(values: Vec<Json>) -> Vec<GenericRecordIn> {
    values
        .into_iter()
        .map(|value| serde_json::from_value(value).expect("generic record deserializes"))
        .collect()
}

fn manifest_records() -> Vec<GenericRecordIn> {
    let root = "urn:sophia:domain:test-domain:manifest";
    let tier = format!("{root}:tier:T1");
    let mode = format!("{root}:mode:local");
    let role = format!("{root}:role:owner");
    let capability = format!("{root}:capability:TEST-C001");
    let journey = format!("{root}:journey:TEST-J001");
    let step = format!("{journey}:step:0-prove");
    records(vec![
        json!({
            "kind": "DomainManifest", "localId": root,
            "programId": "test-domain", "title": "Test domain", "schemaVersion": 1,
            "contentSha256": SHA, "sourceJson": "{\"programId\":\"test-domain\"}",
            "hasTier": [tier], "hasMode": [mode], "hasRole": [role],
            "hasCapability": [capability], "hasJourney": [journey]
        }),
        json!({ "kind": "Tier", "localId": tier, "stableId": "T1", "description": "runtime proof" }),
        json!({ "kind": "Mode", "localId": mode, "stableId": "local" }),
        json!({ "kind": "Role", "localId": role, "stableId": "owner" }),
        json!({
            "kind": "CapabilityClaim", "localId": capability,
            "stableId": "TEST-C001", "domain": "test", "title": "Prove the seam",
            "tier": tier, "stateful": true, "mode": [mode], "role": [role],
            "journey": [journey]
        }),
        json!({
            "kind": "Journey", "localId": journey, "stableId": "TEST-J001",
            "title": "Traverse the seam", "order": 1, "tier": tier,
            "mode": [mode], "role": [role], "exercisesCapability": [capability],
            "declaresEvidence": ["receipt"], "hasStep": [step]
        }),
        json!({
            "kind": "JourneyStep", "localId": step, "stableId": "prove", "order": 0,
            "action": "ingest", "expected": "converges", "declaresEvidence": ["receipt"]
        }),
    ])
}

fn verdict_records(outcome: &str) -> Vec<GenericRecordIn> {
    let verdict = format!("urn:sophia:domain:test:verdict:{SHA}");
    let evidence = format!("urn:sophia:domain:evidence:{SHA}");
    records(vec![
        json!({
            "kind": "Verdict", "localId": verdict, "domain": "test",
            "capabilityId": "TEST-C001", "modeId": "local", "roleId": "owner",
            "targetSha256": SHA, "outcome": outcome,
            "asOf": "2026-08-03T12:00:00Z",
            "agentSession": "urn:sophia:agent:agent-aaaaaaaaaaaaaaaa:session:wfr-1",
            "contentSha256": SHA, "sourceJson": "{\"outcome\":\"PASS\"}",
            "evidence": [evidence]
        }),
        json!({
            "kind": "EvidenceRef", "localId": evidence, "hash": SHA,
            "contentUri": "s3://sophia-evidence/sha256/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "contentSha256": SHA, "mediaType": "application/zip"
        }),
    ])
}

fn dashboard_records() -> Vec<GenericRecordIn> {
    records(vec![json!({
        "kind": "DashboardSurface",
        "localId": "urn:sophia:ux:surface:test-domain",
        "layoutJson": "{\"schemaVersion\":1}",
        "queryCatalogueDigest": SHA,
        "freshnessQuery": "urn:sophia:query:test.freshness",
        "freshnessPolicy": "unknown-on-missing-or-error",
        "freshnessGeneratedAt": "2026-08-03T12:00:00Z",
        "freshnessMaxAgeSeconds": 86400
    })])
}

#[test]
fn all_domain_kit_projection_packs_plan_and_pass_the_real_shacl_gate() {
    for (name, records) in [
        ("sophia-domain-manifest", manifest_records()),
        (
            "sophia-domain-verdict",
            verdict_records("http://sophia.ai/domain#PASS"),
        ),
        ("sophia-domain-dashboard", dashboard_records()),
    ] {
        let contract = get_vocabulary(name).unwrap_or_else(|| panic!("{name} registered"));
        compile_check_contract(contract).unwrap_or_else(|error| panic!("{name}: {error}"));
        let plan = plan_generic_compute(contract, "test-domain", &records)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(
            plan.routes_to_simple_projection(),
            "{name} uses projection authority"
        );
        assert!(
            !plan.desired_inserts.is_empty(),
            "{name} mints a real desired set"
        );
        validate_desired(&plan.desired_inserts, contract)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

#[test]
fn verdict_outcome_outside_the_closed_set_halts_at_shacl() {
    let contract = get_vocabulary("sophia-domain-verdict").expect("verdict pack");
    let plan = plan_generic_compute(
        contract,
        "test-domain",
        &verdict_records("http://sophia.ai/domain#MAYBE"),
    )
    .expect("the wire shape plans before semantic SHACL");
    let error = validate_desired(&plan.desired_inserts, contract)
        .expect_err("MAYBE is not a Domain Kit verdict outcome");
    assert!(error.starts_with("SHACL:"), "failure is loud: {error}");
}
