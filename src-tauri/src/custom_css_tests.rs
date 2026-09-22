use super::*;
use crate::cell_graph_boundary::{CellGraphBoundary, CellRole};
use yrs::{updates::decoder::Decode, Doc, StateVector, Update};

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn route_state(
    app: &AppHandle,
    dir: &std::path::Path,
) -> std::sync::Arc<crate::loopback_state::LoopbackState> {
    use crate::loopback_state::{LoopbackManifest, LoopbackState};
    use std::sync::Arc;
    Arc::new(LoopbackState {
        app: app.clone(),
        token: "css-synthetic-token".into(),
        manifest: LoopbackManifest {
            runtime_profile: "test",
            bind_host: "127.0.0.1",
            port: 0,
            api_url: String::new(),
            mcp_url: String::new(),
            openapi_url: String::new(),
            token: "css-synthetic-token".into(),
            pid: 0,
            started_at: String::new(),
            manifest_path: String::new(),
            auth_header: "Authorization",
            token_audience: "test",
            token_storage: "memory",
            security_warning: "synthetic only",
            cell_graph_id: None,
            cell_owner: None,
            cell_generation: None,
            cell_registry_revision: None,
            capabilities: vec![],
            token_scope_mode: "session-all",
            token_scopes: vec!["workspace.read"],
            scope_details: vec![],
            grant_profiles: vec![],
        },
        jobs: Arc::new(
            crate::local_jobs::LocalJobRegistry::new(dir.join("css-test-jobs")).unwrap(),
        ),
        services: Arc::new(crate::local_service_host::LocalServiceHost::default()),
        lifecycle: app
            .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
            .inner()
            .clone(),
        cell_graph: Arc::new(CellGraphBoundary::for_test(None)),
    })
}

fn request(inc: &str, revision: u64, before: &str, text: &str, operation: &str) -> CssMutation {
    CssMutation {
        graph_incarnation: inc.into(),
        expected_content_sha256: crate::artifact_text_service::hash(before.as_bytes()),
        expected_revision: revision,
        css_text: text.into(),
        operation_id: operation.into(),
    }
}
fn apply(doc: &Doc, input: &CssMutation) -> Result<Value, String> {
    update(
        &mut doc.transact_mut(),
        "css-test",
        &serde_json::to_value(input).unwrap(),
    )
}
#[test]
fn custom_css_exact_text_cas_aba_replay_and_cold_encoding() {
    let doc = Doc::new();
    let css = "/* é🌱 */\r\n:root { --mn-radius-control: 7px; }\n";
    let first = request("inc-1", 0, "", css, "first");
    assert_eq!(apply(&doc, &first).unwrap()["cssText"], css);
    assert!(apply(&doc, &request("inc-1", 0, "", "changed", "stale")).is_err());
    assert!(apply(&doc, &request("inc-1", 0, "", "changed", "first")).is_err());
    assert_eq!(
        apply(&doc, &request("inc-1", 1, css, "", "reset")).unwrap()["revision"],
        2
    );
    assert!(apply(&doc, &request("inc-1", 0, "", "ABA", "aba")).is_err());
    let replay = apply(&doc, &first).unwrap();
    assert_eq!(replay["cssText"], "");
    assert_eq!(replay["revision"], 2);
    assert_eq!(replay["replayedOperationId"], "first");
    let bytes = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let cold = Doc::new();
    cold.transact_mut()
        .apply_update(Update::decode_v1(&bytes).unwrap())
        .unwrap();
    assert_eq!(apply(&cold, &first).unwrap(), replay);
    assert!(apply(&cold, &request("other-inc", 2, "", "x", "foreign")).is_err());
}
#[test]
fn custom_css_bounds_and_malformed_authority_never_heal() {
    assert!(validate(&request("inc", 0, "", &"x".repeat(MAX_CSS_BYTES), "limit")).is_ok());
    assert!(validate(&request(
        "inc",
        0,
        "",
        &"é".repeat(MAX_CSS_BYTES / 2 + 1),
        "large"
    ))
    .is_err());
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        txn.get_or_insert_map(STATE_MAP)
            .insert(&mut txn, "state", "future");
    }
    let before = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    assert!(apply(&doc, &request("inc", 0, "", "x", "op")).is_err());
    assert_eq!(
        before,
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    );
}
#[test]
fn custom_css_optional_discovery_and_role_boundary() {
    let boundary = CellGraphBoundary::for_test(Some("css-test"));
    let listed = |role, enabled| {
        serde_json::to_value(
            crate::mcp_tool_registry::mcp_tools_list_result_with_optional(&boundary, role, enabled),
        )
        .unwrap()
    };
    let contains = |v: &Value, name: &str| {
        v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == name)
    };
    assert!(!contains(
        &listed(CellRole::Owner, false),
        "read_custom_css"
    ));
    assert!(contains(&listed(CellRole::Owner, true), "write_custom_css"));
    assert!(contains(&listed(CellRole::Viewer, true), "read_custom_css"));
    assert!(!contains(
        &listed(CellRole::Viewer, true),
        "write_custom_css"
    ));
    assert!(boundary
        .authorize_crdt_operation("workspace.setCustomCss", "foreign", &json!({}))
        .is_err());
    assert!(crate::mcp_dispatch_registry::lookup("read_custom_css").is_some());
    assert!(crate::mcp_dispatch_registry::lookup("write_custom_css").is_some());
}
#[cfg(all(feature = "headless", not(feature = "desktop")))]
#[test]
fn custom_css_real_queue_mcp_cas_workspace_notification_and_cold_read() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-css-native-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some("css-native".into()),
                title: "Synthetic CSS".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        crate::app_runtime::async_runtime::block_on(async {
            let graph = "css-native";
            let initial = read(&app, graph).await.unwrap();
            let inc = initial["graphIncarnation"].as_str().unwrap();
            let (dir, _) =
                crate::graph_record_store::read_graph_record_no_heal(&app, graph).unwrap();
            let route = route_state(&app, &dir);
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "authorization",
                axum::http::HeaderValue::from_static("Bearer css-synthetic-token"),
            );
            let response = crate::loopback_custom_css_routes::read(
                axum::extract::State(route.clone()),
                headers.clone(),
                axum::extract::Path(graph.to_string()),
            )
            .await;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
            let wire: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 131072)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(wire, initial);
            assert_eq!(
                crate::loopback_custom_css_routes::write(
                    axum::extract::State(route.clone()),
                    headers.clone(),
                    axum::extract::Path(graph.to_string()),
                    axum::Json(json!({}))
                )
                .await
                .status(),
                axum::http::StatusCode::FORBIDDEN
            );
            let room = workspace_ops::workspace_room(&app, graph, &dir)
                .await
                .unwrap();
            let mut observer = room.subscribe_updates_for_test();
            let input = request(inc, 0, "", ":root { --mn-radius-control: 9px; }", "first");
            let mut args = serde_json::to_value(&input).unwrap();
            args["graph_id"] = json!(graph);
            let context = crate::mcp_dispatch_registry::McpCallCtx {
                app: app.clone(),
                jobs: &route.jobs,
            };
            let first = (crate::mcp_dispatch_registry::lookup("write_custom_css")
                .unwrap()
                .handler)(&context, &args)
            .await
            .unwrap();
            assert_eq!(first["revision"], 1);
            assert!(observer.try_recv().is_ok());
            assert_eq!(
                mcp_read(app.clone(), &json!({"graph_id":graph}))
                    .await
                    .unwrap()["cssText"],
                input.css_text
            );
            let mut stale = input.clone();
            stale.operation_id = "stale".into();
            assert!(submit(app.clone(), graph.into(), stale).await.is_err());
            let left = request(inc, 1, &input.css_text, "left", "left");
            let right = request(inc, 1, &input.css_text, "right", "right");
            let (a, b) = tokio::join!(
                submit(app.clone(), graph.into(), left),
                submit(app.clone(), graph.into(), right)
            );
            assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
            let current = read(&app, graph).await.unwrap();
            let reset = request(inc, 2, current["cssText"].as_str().unwrap(), "", "reset");
            assert_eq!(
                submit(app.clone(), graph.into(), reset).await.unwrap()["revision"],
                3
            );
            let replay = submit(app.clone(), graph.into(), input.clone())
                .await
                .unwrap();
            assert_eq!(replay["cssText"], "");
            assert_eq!(replay["revision"], 3);
            let mut foreign = input.clone();
            foreign.graph_incarnation = "foreign-inc".into();
            assert!(submit(app.clone(), graph.into(), foreign).await.is_err());
            let bytes = std::fs::read(crate::paths::workspace_ydoc_state_path(&dir)).unwrap();
            let cold = Doc::new();
            cold.transact_mut()
                .apply_update(Update::decode_v1(&bytes).unwrap())
                .unwrap();
            assert_eq!(
                snapshot(graph, &read_state(&cold.transact(), inc).unwrap()),
                read(&app, graph).await.unwrap()
            );
            println!(
                "CSS_SYNTHETIC_WITNESS={}",
                json!({"profile":profile,"current":read(&app,graph).await.unwrap(),"workspaceBytes":bytes.len(),"coldYDocRead":true,"physicalRestart":false})
            );
        });
    });
    if let Some(previous) = previous {
        std::env::set_var("GARDEN_PROFILE_DIR", previous);
    } else {
        std::env::remove_var("GARDEN_PROFILE_DIR");
    }
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}
