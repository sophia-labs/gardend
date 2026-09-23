//! Real tests for `agent_self_image`: the live `sophia-agent-core` SHACL, a real
//! oxigraph store, real SVG bytes, and (headless) the real MCP dispatch against
//! an on-disk profile. The provider leg is an integration test that runs only
//! when `SOPHIA_OPENROUTER_API_KEY` is present in the environment (the same
//! variable a hosted cell reads).
use super::*;

const AGENT: &str = "agent-0123456789abcdef";
const OTHER: &str = "agent-fedcba9876543210";
const MODE: &str = "urn:sophia:mode:researcher";

fn image<'a>(
    agent_id: &'a str,
    sha: &'a str,
    variant: Option<(&'a str, &'a str, &'a str)>,
) -> NewImage<'a> {
    NewImage {
        agent_id,
        artifact_id: "agent-self-image-0123456789abcdef",
        revision_id: "rev-1-abc",
        content_sha256: sha,
        mime_type: "image/svg+xml",
        prompt: "a small green archivist",
        generator: LOCAL_GENERATOR,
        model: LOCAL_MODEL,
        generated_at: "2026-09-22T12:00:00.000Z",
        principal: Some("user:vera"),
        variant,
    }
}

fn store_with_agent_and_mode(graph: &str, transform: &str) -> Store {
    let store = Store::new().unwrap();
    let user = user_graph(graph);
    crate::rdf_query_service::execute_sparql_update(
        &store,
        &format!(
            r#"INSERT DATA {{ GRAPH <{user}> {{
  <urn:sophia:agent:{AGENT}> a <{AGT}Agent> ; <{AGT}agentId> "{AGENT}" ; <{RDFS_LABEL}> "Scout" ; <{AGT}defaultMode> <{MODE}> .
  <urn:sophia:agent:{OTHER}> a <{AGT}Agent> ; <{AGT}agentId> "{OTHER}" .
  <{MODE}> a <{AGT}Mode> ; <{RDFS_LABEL}> "Researcher" ; <{AGT}imageTransform> "{transform}" .
}} }}"#
        ),
    )
    .unwrap();
    store
}

#[test]
fn identities_are_per_agent_and_the_derivation_key_tracks_every_input() {
    assert!(agent_id_valid(AGENT));
    assert!(!agent_id_valid("agent-XYZ"));
    assert_eq!(
        base_image_iri(AGENT),
        format!("urn:sophia:agent:{AGENT}#self-image")
    );
    assert!(variant_image_iri(AGENT, MODE).starts_with(&base_image_iri(AGENT)));
    assert_ne!(
        variant_image_iri(AGENT, MODE),
        variant_image_iri(AGENT, "urn:sophia:mode:editor")
    );
    assert_ne!(
        variant_artifact_id(AGENT, MODE),
        variant_artifact_id(OTHER, MODE)
    );
    for id in [base_artifact_id(AGENT), variant_artifact_id(AGENT, MODE)] {
        crate::ids::validate_local_id(&id, "artifact_id").unwrap();
    }
    let key = derivation_key("aa", "wearing glasses", LOCAL_GENERATOR, LOCAL_MODEL);
    assert_eq!(
        key,
        derivation_key("aa", "wearing glasses", LOCAL_GENERATOR, LOCAL_MODEL)
    );
    assert_ne!(
        key,
        derivation_key("ab", "wearing glasses", LOCAL_GENERATOR, LOCAL_MODEL)
    );
    assert_ne!(
        key,
        derivation_key("aa", "wearing a hat", LOCAL_GENERATOR, LOCAL_MODEL)
    );
    assert_ne!(
        key,
        derivation_key("aa", "wearing glasses", OPENROUTER_GENERATOR, LOCAL_MODEL)
    );
}

#[test]
fn the_local_sigil_is_deterministic_well_formed_svg_and_the_mode_dresses_the_base() {
    let a = sigil_base(AGENT, Some("Scout <&>"), "a small green archivist");
    let b = sigil_base(AGENT, Some("Scout <&>"), "a small green archivist");
    let c = sigil_base(AGENT, Some("Scout <&>"), "a tall red critic");
    assert_eq!(a.bytes, b.bytes, "same inputs, same bytes");
    assert_ne!(a.bytes, c.bytes, "the prompt shapes the face");
    let text = String::from_utf8(a.bytes.clone()).unwrap();
    roxmltree::Document::parse(&text).expect("base sigil is well-formed XML");
    assert!(text.contains("Scout &lt;&amp;&gt;"), "labels are escaped");

    let glasses = sigil_variant(
        &a.mime_type,
        &a.bytes,
        Some("Researcher"),
        "wearing round reading glasses",
    );
    let vtext = String::from_utf8(glasses.bytes.clone()).unwrap();
    roxmltree::Document::parse(&vtext).expect("variant is well-formed XML");
    assert!(vtext.contains(r#"data-prop="glasses""#));
    assert!(
        vtext.contains(&BASE64_STANDARD.encode(&a.bytes)),
        "the variant embeds the exact base bytes"
    );
    assert!(vtext.contains(">Researcher<"));
    let other = sigil_variant(&a.mime_type, &a.bytes, None, "glowing with quiet resolve");
    let otext = String::from_utf8(other.bytes).unwrap();
    assert!(
        !otext.contains(r#"data-prop="glasses""#),
        "no prop it cannot draw"
    );
    assert!(
        otext.contains(">glowing with quiet resolve<"),
        "unknown transforms become an honest ribbon"
    );
}

#[test]
fn base_and_variant_records_pass_the_live_agent_core_shapes() {
    let (_, base) = record_triples(&image(AGENT, "aa", None)).unwrap();
    validate_record(&base, &[], "lab").expect("base self-image conforms");
    let base_iri = base_image_iri(AGENT);
    let (subject, variant) =
        record_triples(&image(AGENT, "bb", Some((&base_iri, MODE, "kk")))).unwrap();
    assert_eq!(subject, variant_image_iri(AGENT, MODE));
    validate_record(&variant, &base, "lab").expect("variant with its own base conforms");
}

#[test]
fn the_shapes_refuse_a_variant_without_lineage_or_dressed_in_another_agents_image() {
    let base_iri = base_image_iri(AGENT);
    // (a) a variant with no derivedFromImage / derivationKey
    let (subject, mut orphan) =
        record_triples(&image(AGENT, "bb", Some((&base_iri, MODE, "kk")))).unwrap();
    orphan.retain(|(_, p, _)| p != &agt("derivedFromImage") && p != &agt("derivationKey"));
    let error = validate_record(&orphan, &[], "lab").expect_err("orphan variant must halt");
    assert!(error.to_string().contains(&subject), "{error}");
    // (b) AGENT's variant claiming OTHER's base image
    let (_, other_base) = record_triples(&image(OTHER, "cc", None)).unwrap();
    let other_iri = base_image_iri(OTHER);
    let (_, stolen) = record_triples(&image(AGENT, "bb", Some((&other_iri, MODE, "kk")))).unwrap();
    validate_record(&stolen, &other_base, "lab")
        .expect_err("a variant derived from another agent's image must halt");
    // (c) a rogue predicate trips the derived closed SelfImage shape
    let (s, mut rogue) = record_triples(&image(AGENT, "aa", None)).unwrap();
    rogue.push((
        s,
        "http://example.org/not-in-the-contract".into(),
        str_term("x"),
    ));
    validate_record(&rogue, &[], "lab").expect_err("closed shape refuses rogue predicates");
}

#[test]
fn writes_touch_only_the_agents_own_subjects_and_read_back_exactly() {
    let store = store_with_agent_and_mode("lab", "wearing round reading glasses");
    assert_eq!(
        agent_in_graph(&store, AGENT).unwrap(),
        Some(Some("Scout".into()))
    );
    assert_eq!(
        agent_in_graph(&store, "agent-1111111111111111").unwrap(),
        None
    );
    let mode = read_mode(&store, MODE).unwrap();
    assert_eq!(mode.transform, "wearing round reading glasses");
    assert_eq!(default_mode(&store, AGENT).unwrap().as_deref(), Some(MODE));

    let (subject, triples) = record_triples(&image(AGENT, "aa", None)).unwrap();
    write_record(&store, "lab", AGENT, &subject, &triples, true).unwrap();
    let record = read_self_image(&store, "lab", &subject)
        .unwrap()
        .expect("record");
    assert_eq!(record.get("contentSha256"), Some("aa"));
    assert_eq!(record.get("imageOf"), Some(agent_iri(AGENT).as_str()));
    // oxigraph stores the canonical xsd:dateTime lexical form.
    assert_eq!(record.get("generatedAtTime"), Some("2026-09-22T12:00:00Z"));
    let link = subject_fields(&store, &agent_iri(AGENT)).unwrap();
    assert_eq!(
        first(&link, &agt("selfImage")).as_deref(),
        Some(subject.as_str())
    );

    // Regenerate: the subject is replaced, not accumulated.
    let (_, again) = record_triples(&image(AGENT, "ab", None)).unwrap();
    write_record(&store, "lab", AGENT, &subject, &again, true).unwrap();
    let record = read_self_image(&store, "lab", &subject).unwrap().unwrap();
    assert_eq!(record.get("contentSha256"), Some("ab"));
    assert_eq!(
        store
            .quads_for_pattern(
                Some(NamedNodeRef::new_unchecked(&subject).into()),
                Some(NamedNodeRef::new_unchecked(&agt("contentSha256"))),
                None,
                None
            )
            .count(),
        1
    );
    // The stored record round-trips into triples that still conform.
    validate_record(&stored_triples(&record).unwrap(), &[], "lab").unwrap();

    // Self-only: AGENT may not write OTHER's image subject.
    let (other_subject, other) = record_triples(&image(OTHER, "cc", None)).unwrap();
    write_record(&store, "lab", AGENT, &other_subject, &other, true)
        .expect_err("an agent writes only its own self-image");
    assert!(read_self_image(&store, "lab", &other_subject)
        .unwrap()
        .is_none());
}

#[test]
fn a_hosted_principal_cannot_replace_an_image_another_principal_wrote() {
    let mut record = SelfImageRecord {
        iri: base_image_iri(AGENT),
        ..Default::default()
    };
    record
        .fields
        .insert("ingressPrincipal".into(), "user:vera".into());
    guard_principal(Some(&record), Some("user:vera")).unwrap();
    guard_principal(Some(&record), None).unwrap();
    guard_principal(None, Some("user:mallory")).unwrap();
    let error = guard_principal(Some(&record), Some("user:mallory")).unwrap_err();
    assert!(error.to_string().contains("different principal"), "{error}");
}

#[test]
fn a_mode_without_an_image_transform_or_a_non_mode_is_refused() {
    let store = store_with_agent_and_mode("lab", "wearing a hat");
    assert!(read_mode(&store, &agent_iri(AGENT)).is_err());
    crate::rdf_query_service::execute_sparql_update(
        &store,
        &format!("INSERT DATA {{ <urn:sophia:mode:bare> a <{AGT}Mode> }}"),
    )
    .unwrap();
    let error = read_mode(&store, "urn:sophia:mode:bare").unwrap_err();
    assert!(error.to_string().contains("imageTransform"), "{error}");
}

#[test]
fn parse_refuses_bad_arguments() {
    let ok =
        serde_json::json!({"graph_id":"lab","agentId":AGENT,"action":"generate","prompt":"me"});
    assert!(parse(&ok).is_ok());
    for bad in [
        serde_json::json!({"graph_id":"lab","agentId":"agent-nothex","action":"current"}),
        serde_json::json!({"graph_id":"lab","agentId":AGENT,"action":"delete"}),
        serde_json::json!({"graph_id":"lab","agentId":AGENT,"generator":"dall-e"}),
        serde_json::json!({"graph_id":"lab","agentId":AGENT,"subject":"urn:x"}),
        serde_json::json!({"graph_id":"lab","graphId":"foreign","agentId":AGENT}),
        serde_json::json!({"graph_id":"lab","agentId":AGENT,"modeIri":"not an iri"}),
    ] {
        assert!(parse(&bad).is_err(), "must refuse {bad}");
    }
}

/// The organism: the REAL registered MCP handler over a real on-disk profile.
/// Generate a base self-image with the deterministic local generator, derive the
/// Researcher variant, prove the cache, prove staleness on transform change and
/// on base regeneration, and prove the bytes are ordinary artifact revisions.
/// Set `AGENT_SELF_IMAGE_DEMO_OUT=<dir>` to keep the SVGs (the demo).
#[cfg(all(feature = "headless", not(feature = "desktop")))]
#[test]
fn agent_self_image_real_mcp_generate_variant_cache_and_staleness() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-self-image-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph = "self-image-lab";
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(graph.into()),
                title: "Self image lab".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        crate::app_runtime::async_runtime::block_on(async {
            let (dir, _) =
                crate::graph_record_store::read_graph_record_no_heal(&app, graph).unwrap();
            let store = crate::rdf_service::open_graph_store(&dir).unwrap();
            let seed = store_with_agent_and_mode(graph, "wearing round reading glasses");
            for quad in seed.iter() {
                store.insert(&quad.unwrap()).unwrap();
            }
            drop(store);
            let jobs = std::sync::Arc::new(
                crate::local_jobs::LocalJobRegistry::new(profile.join("jobs")).unwrap(),
            );
            let ctx = crate::mcp_dispatch_registry::McpCallCtx {
                app: app.clone(),
                jobs: &jobs,
            };
            let tool = crate::mcp_dispatch_registry::lookup(TOOL_NAME)
                .expect("registered")
                .handler;
            let call = |args: Value| {
                let mut args = args;
                args["graph_id"] = json!(graph);
                args["agentId"] = json!(AGENT);
                args
            };

            // No base yet.
            let empty = tool(&ctx, &call(json!({"action":"current"})))
                .await
                .unwrap();
            assert!(empty["selfImage"].is_null());
            assert_eq!(empty["modeVariantStatus"], "missing");
            assert!(tool(
                &ctx,
                &call(json!({"action":"mode_variant","generator":"local-sigil"}))
            )
            .await
            .is_err());
            // An agent absent from the graph gets nothing.
            let mut ghost =
                call(json!({"action":"generate","prompt":"x","generator":"local-sigil"}));
            ghost["agentId"] = json!("agent-1111111111111111");
            assert!(tool(&ctx, &ghost).await.is_err());

            // Generate the base.
            let base = tool(&ctx, &call(json!({"action":"generate","prompt":"a small green archivist who loves footnotes","generator":"local-sigil"}))).await.unwrap();
            let base_sha = base["selfImage"]["contentSha256"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(base["selfImage"]["generator"], LOCAL_GENERATOR);
            assert_eq!(base["selfImage"]["generatedByTool"], TOOL_NAME);
            let artifact = base_artifact_id(AGENT);
            assert_eq!(base["selfImage"]["artifactId"], artifact);

            // The bytes are an ordinary artifact: read_artifact + revisions agree.
            let read = (crate::mcp_dispatch_registry::lookup("read_artifact")
                .unwrap()
                .handler)(
                &ctx, &json!({"graph_id":graph,"artifact_id":artifact})
            )
            .await
            .unwrap();
            assert_eq!(read["kind"], "image");
            let bytes = BASE64_STANDARD
                .decode(read["dataBase64"].as_str().unwrap())
                .unwrap();
            assert_eq!(sha256_hex(&bytes), base_sha);
            let revisions =
                crate::artifact_revisions::list_artifact_revisions(&app, graph, &artifact).unwrap();
            assert_eq!(revisions.len(), 1);
            assert_eq!(
                revisions[0].revision_id,
                base["selfImage"]["artifactRevision"]
            );

            // Derive the Researcher variant (default mode), then hit the cache.
            let first = tool(
                &ctx,
                &call(json!({"action":"mode_variant","generator":"local-sigil"})),
            )
            .await
            .unwrap();
            assert_eq!(first["cached"], false);
            assert_eq!(
                first["selfImage"]["derivedFromImage"],
                base_image_iri(AGENT)
            );
            assert_eq!(first["selfImage"]["underMode"], MODE);
            let again = tool(
                &ctx,
                &call(json!({"action":"mode_variant","modeIri":MODE,"generator":"local-sigil"})),
            )
            .await
            .unwrap();
            assert_eq!(again["cached"], true);
            assert_eq!(
                again["selfImage"]["artifactRevision"],
                first["selfImage"]["artifactRevision"]
            );
            let current = tool(&ctx, &call(json!({"action":"current","includeImage":true})))
                .await
                .unwrap();
            assert_eq!(current["modeVariantStatus"], "fresh");
            let variant_svg = String::from_utf8(
                BASE64_STANDARD
                    .decode(current["modeVariantImage"]["dataBase64"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap();
            assert!(variant_svg.contains(r#"data-prop="glasses""#));
            assert!(variant_svg.contains(&BASE64_STANDARD.encode(&bytes)));
            let describe = tool(&ctx, &call(json!({"action":"describe"})))
                .await
                .unwrap();
            assert!(describe["description"]
                .as_str()
                .unwrap()
                .contains("reading glasses"));

            if let Some(out) = std::env::var_os("AGENT_SELF_IMAGE_DEMO_OUT") {
                let out = std::path::PathBuf::from(out);
                std::fs::create_dir_all(&out).unwrap();
                std::fs::write(out.join("scout-base.svg"), &bytes).unwrap();
                std::fs::write(out.join("scout-researcher.svg"), variant_svg.as_bytes()).unwrap();
                std::fs::write(
                    out.join("scout-describe.json"),
                    serde_json::to_vec_pretty(&describe).unwrap(),
                )
                .unwrap();
            }

            // Change the mode's transform → stale → regenerated with a new key.
            {
                let store = crate::rdf_service::open_graph_store(&dir).unwrap();
                crate::rdf_query_service::execute_sparql_update(
                    &store,
                    &format!("DELETE WHERE {{ GRAPH ?g {{ <{MODE}> <{AGT}imageTransform> ?t }} }}; INSERT DATA {{ GRAPH <{}> {{ <{MODE}> <{AGT}imageTransform> \"wearing a tall hat\" }} }}", user_graph(graph)),
                )
                .unwrap();
            }
            let stale = tool(&ctx, &call(json!({"action":"current"})))
                .await
                .unwrap();
            assert_eq!(stale["modeVariantStatus"], "stale");
            let hat = tool(
                &ctx,
                &call(json!({"action":"mode_variant","generator":"local-sigil"})),
            )
            .await
            .unwrap();
            assert_eq!(hat["cached"], false);
            assert_ne!(
                hat["selfImage"]["derivationKey"],
                first["selfImage"]["derivationKey"]
            );
            let variant_artifact = variant_artifact_id(AGENT, MODE);
            assert_eq!(
                crate::artifact_revisions::list_artifact_revisions(&app, graph, &variant_artifact)
                    .unwrap()
                    .len(),
                2,
                "each derivation is a revision of the same variant artifact"
            );

            // Regenerate the base → the variant is stale again.
            tool(&ctx, &call(json!({"action":"generate","prompt":"a small blue archivist","generator":"local-sigil"}))).await.unwrap();
            let after = tool(&ctx, &call(json!({"action":"current"})))
                .await
                .unwrap();
            assert_eq!(after["modeVariantStatus"], "stale");

            // The provider path refuses cleanly without a key (only provable
            // when no env var / Keychain item resolves on this machine).
            if crate::local_provider_keys::provider_key(&app, "openrouter").is_none() {
                let error = tool(&ctx, &call(json!({"action":"generate","prompt":"me"})))
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("SOPHIA_OPENROUTER_API_KEY"), "{error}");
            }
        });
    });
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// INTEGRATION (live provider): runs only when `SOPHIA_OPENROUTER_API_KEY` is
/// set in the environment; otherwise it reports the skip and returns. The cell
/// resolves the key through its REAL first source (the env var, exactly as a
/// Sirin cell does); the value is never printed or written anywhere.
#[cfg(all(feature = "headless", not(feature = "desktop")))]
#[test]
fn agent_self_image_openrouter_integration_when_credentials_are_present() {
    if std::env::var("SOPHIA_OPENROUTER_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .is_none()
    {
        eprintln!(
            "SKIP agent_self_image_openrouter_integration: SOPHIA_OPENROUTER_API_KEY not set"
        );
        return;
    }
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-self-image-live-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph = "self-image-live";
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(graph.into()),
                title: "Self image live".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        crate::app_runtime::async_runtime::block_on(async {
            let (dir, _) =
                crate::graph_record_store::read_graph_record_no_heal(&app, graph).unwrap();
            let store = crate::rdf_service::open_graph_store(&dir).unwrap();
            for quad in store_with_agent_and_mode(graph, "wearing round reading glasses").iter() {
                store.insert(&quad.unwrap()).unwrap();
            }
            drop(store);
            let jobs = std::sync::Arc::new(
                crate::local_jobs::LocalJobRegistry::new(profile.join("jobs")).unwrap(),
            );
            let ctx = crate::mcp_dispatch_registry::McpCallCtx {
                app: app.clone(),
                jobs: &jobs,
            };
            let tool = crate::mcp_dispatch_registry::lookup(TOOL_NAME)
                .unwrap()
                .handler;
            let base = tool(&ctx, &json!({"graph_id":graph,"agentId":AGENT,"action":"generate","prompt":"a small green archivist who loves footnotes, flat illustration"})).await.unwrap();
            assert_eq!(base["selfImage"]["generator"], OPENROUTER_GENERATOR);
            assert!(base["selfImage"]["mimeType"]
                .as_str()
                .unwrap()
                .starts_with("image/"));
            let variant = tool(
                &ctx,
                &json!({"graph_id":graph,"agentId":AGENT,"action":"mode_variant"}),
            )
            .await
            .unwrap();
            assert_eq!(variant["cached"], false);
            let current = tool(
                &ctx,
                &json!({"graph_id":graph,"agentId":AGENT,"action":"current","includeImage":true}),
            )
            .await
            .unwrap();
            assert_eq!(current["modeVariantStatus"], "fresh");
            if let Some(out) = std::env::var_os("AGENT_SELF_IMAGE_DEMO_OUT") {
                let out = std::path::PathBuf::from(out);
                std::fs::create_dir_all(&out).unwrap();
                for (key, name) in [
                    ("image", "scout-base-live"),
                    ("modeVariantImage", "scout-researcher-live"),
                ] {
                    let mime = current[key]["mimeType"].as_str().unwrap();
                    let bytes = BASE64_STANDARD
                        .decode(current[key]["dataBase64"].as_str().unwrap())
                        .unwrap();
                    std::fs::write(out.join(format!("{name}.{}", extension(mime))), bytes).unwrap();
                }
            }
        });
    });
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
