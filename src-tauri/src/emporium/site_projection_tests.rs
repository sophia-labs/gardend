//! Contract and durable-write proofs for the registered `shrubbery-site` pack.
//!
//! Phanes supplies the authority precedent: an agent proposal is data, while
//! the durable host binds the exact target and drives dry-run/apply/replay.
//! These tests therefore use Garden's ordinary Emporium planner, SHACL gate,
//! and (under `headless`) write surface rather than a site-specific RDF writer.

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
        .map(|value| serde_json::from_value(value).expect("site record deserializes"))
        .collect()
}

fn site_definition() -> Json {
    json!({
        "kind": "SiteDefinition",
        "localId": "garden",
        "bundleId": "garden",
        "bundleVersion": "0.1.0-local",
        "interpreterPackage": "@shrubbery/planter",
        "interpreterVersion": "0.0.0",
        "layoutSeedSha256": SHA,
    })
}

fn content_source(auth_mode: &str, liveness: &str) -> Json {
    json!({
        "kind": "ContentSource",
        "localId": "hosted-gateway",
        "endpoint": "https://api.canary.sophia-labs.com",
        "ownerPrincipal": "user:specialist-sub",
        "graphId": "shrubbery-domain",
        "graphIri": "urn:mnemosyne:local:graph:shrubbery-domain:ux:config",
        "authMode": auth_mode,
        "liveness": liveness,
        "observer": "planter-pool",
        "tripleCount": 0,
    })
}

fn publication_route(state: &str, approved: bool) -> Json {
    let mut route = json!({
        "kind": "PublicationRoute",
        "localId": "shrubbery-preview",
        "publicName": "shrubbery-preview",
        "pathPrefix": "/",
        "ownerPrincipal": "user:specialist-sub",
        "graphId": "shrubbery-domain",
        "siteDefinition": "urn:mnemosyne:local:graph:shrubbery-domain:projection:site:definition:garden",
        "interpreterPackage": "@shrubbery/planter",
        "interpreterVersion": "0.0.0",
        "publicationState": state,
    });
    if approved {
        route["approvedBy"] = json!("user:vera");
        route["approvedAt"] = json!("2026-08-03T12:00:00Z");
    }
    route
}

#[test]
fn site_definition_and_exact_unlisted_route_use_the_real_planner_and_shacl_gate() {
    let contract = get_vocabulary("shrubbery-site").expect("site pack is registered");
    compile_check_contract(contract).expect("site shapes compile in the real engine");
    assert_eq!(contract.write_target.as_deref(), Some("projection:site"));

    for (graph_id, input) in [
        ("shrubbery-domain", site_definition()),
        ("platform-authority", publication_route("unlisted", false)),
    ] {
        let plan = plan_generic_compute(contract, graph_id, &records(vec![input]))
            .unwrap_or_else(|error| panic!("{graph_id}: {error}"));
        assert!(plan.routes_to_simple_projection());
        assert!(!plan.desired_inserts.is_empty());
        validate_desired(&plan.desired_inserts, contract)
            .unwrap_or_else(|error| panic!("{graph_id}: {error}"));
    }
}

#[test]
fn delegated_content_source_is_testimony_not_a_credential_or_open_enum() {
    let contract = get_vocabulary("shrubbery-site").expect("site pack is registered");

    let delegated = plan_generic_compute(
        contract,
        "shrubbery-domain",
        &records(vec![content_source("service", "poll")]),
    )
    .expect("delegated service testimony plans");
    validate_desired(&delegated.desired_inserts, contract)
        .expect("registered service/poll testimony satisfies SHACL");
    let rendered = format!("{:?}", delegated.desired_inserts);
    assert!(rendered.contains("service"));
    assert!(
        !rendered.contains("Bearer "),
        "credentials are never represented"
    );

    for malformed in [
        content_source("bearer", "poll"),
        content_source("service", "stream"),
    ] {
        let plan = plan_generic_compute(contract, "shrubbery-domain", &records(vec![malformed]))
            .expect("wire shape plans before semantic SHACL");
        validate_desired(&plan.desired_inserts, contract)
            .expect_err("open auth or liveness testimony must halt");
    }
}

#[test]
fn public_route_requires_human_testimony_and_exact_target_grammar() {
    let contract = get_vocabulary("shrubbery-site").expect("site pack is registered");

    let unapproved = plan_generic_compute(
        contract,
        "platform-authority",
        &records(vec![publication_route("published", false)]),
    )
    .expect("wire shape plans before semantic SHACL");
    let error = validate_desired(&unapproved.desired_inserts, contract)
        .expect_err("published without user approval must halt");
    assert!(error.starts_with("SHACL:"), "failure is loud: {error}");

    let mut malformed = publication_route("unlisted", false);
    malformed["ownerPrincipal"] = json!("specialist-sub");
    malformed["graphId"] = json!("Shrubbery-Domain");
    let malformed = plan_generic_compute(contract, "platform-authority", &records(vec![malformed]))
        .expect("wire shape plans before semantic SHACL");
    validate_desired(&malformed.desired_inserts, contract)
        .expect_err("ownerless and non-canonical target must halt");

    let approved = plan_generic_compute(
        contract,
        "platform-authority",
        &records(vec![publication_route("published", true)]),
    )
    .expect("approved route plans");
    validate_desired(&approved.desired_inserts, contract)
        .expect("explicit user approval satisfies publication SHACL");
}

#[cfg(feature = "headless")]
mod headless {
    use super::*;
    use crate::{
        app_runtime::AppHandle,
        emporium::{objects::read_object, write::emporium_write},
        graph_service::{create_graph_service, CreateGraphInput},
    };
    use std::{
        path::PathBuf,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn env_serial() -> &'static Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-site-pack-{name}-{nanos}"))
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Site Pack Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    #[test]
    fn host_dry_run_apply_and_replay_preserve_the_exact_site_route() {
        let _serial = env_serial()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let profile = temp_profile("authority-loop");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let target_graph = "shrubbery-domain";
            let registry_graph = "platform-authority";
            seed_graph(&app, target_graph);
            seed_graph(&app, registry_graph);

            let preview = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                target_graph,
                "shrubbery-site",
                &[site_definition()],
                true,
                false,
                None,
            ))
            .expect("definition dry-run succeeds");
            assert_eq!(preview["ok"], json!(true), "{preview}");
            assert!(preview["journalRef"].is_null(), "{preview}");
            assert!(
                read_object(
                    &app,
                    target_graph,
                    "shrubbery-site",
                    "SiteDefinition",
                    "garden",
                )
                .is_err(),
                "dry-run writes nothing"
            );

            let definition = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                target_graph,
                "shrubbery-site",
                &[site_definition()],
                false,
                false,
                None,
            ))
            .expect("definition applies");
            assert_eq!(definition["ok"], json!(true), "{definition}");
            assert!(definition["journalRef"].is_string(), "{definition}");

            let route_record = publication_route("unlisted", false);
            let route = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                registry_graph,
                "shrubbery-site",
                &[route_record.clone()],
                false,
                false,
                None,
            ))
            .expect("exact route applies");
            assert_eq!(route["ok"], json!(true), "{route}");

            let replay = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                registry_graph,
                "shrubbery-site",
                &[route_record],
                false,
                false,
                None,
            ))
            .expect("identical host replay converges without duplication");
            assert_eq!(
                replay["results"][0]["subject"],
                route["results"][0]["subject"]
            );

            let stored = read_object(
                &app,
                registry_graph,
                "shrubbery-site",
                "PublicationRoute",
                "shrubbery-preview",
            )
            .expect("route reads back");
            let owners = stored["predicates"]["http://sophia.ai/site#ownerPrincipal"]
                .as_array()
                .expect("owner values");
            assert_eq!(owners.len(), 1, "replay did not duplicate owner: {stored}");
            assert!(owners[0]
                .as_str()
                .unwrap_or_default()
                .contains("user:specialist-sub"));

            let refused = crate::app_runtime::async_runtime::block_on(emporium_write(
                &app,
                registry_graph,
                "shrubbery-site",
                &[publication_route("published", false)],
                true,
                false,
                None,
            ))
            .expect("dry-run returns a typed refusal");
            assert_eq!(refused["ok"], json!(false), "{refused}");
            assert_eq!(
                refused["results"][0]["outcome"],
                json!("halted"),
                "{refused}"
            );
        });
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
