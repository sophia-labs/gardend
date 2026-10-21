//! The narrow grant for agent memory: what a supervised local service may
//! write of the Geist of the agents it runs, and nothing else of memory.
//!
//! A self-hosted Compute holds the member grant, which has no `memory.write`
//! or `salience.write` (`loopback_token_grants`). A service whose `service.json`
//! says `"agentMemory": true` also holds the two scopes below. Each admits a
//! write tool only for a call that names an agent as its observer
//! (`observer_agent_id`, an agent id: `agent-` and 16 lowercase hex digits), so
//! the write lands in that agent's own witness graphs
//! (`…:projection:{memory|song|salience}:agent:{id}`) and never in the commons.
//! Which tool each admits is in the effect registry
//! (`parity/local-loopback-surface.json`, `observedScopes`), beside the full
//! scope it stands in for; `mcp_tool_dispatch` checks it on every call.
//!
//! Only a service token can carry them. They are not in the loopback scope
//! catalog (`loopback_scope_catalog`), so no grant profile has them
//! (`session-all`, the master key's, included) and a client token cannot be
//! minted with them ("unknown loopback scope"). A delegation token carries
//! the scopes of the person it acts for, never the service's.

use crate::loopback_scope_types::{scope, LoopbackScopeDetail};

pub(crate) const MEMORY_WRITE_OBSERVED: &str = "memory.write.observed";
pub(crate) const SALIENCE_WRITE_OBSERVED: &str = "salience.write.observed";

/// The two service-only scopes, described as the catalog describes its own.
pub(crate) const AGENT_MEMORY_SCOPES: &[LoopbackScopeDetail] = &[
    scope(
        MEMORY_WRITE_OBSERVED,
        "memory",
        "write",
        "Write the memory and Song of the agent a call names as its observer (observer_agent_id: agent- and 16 hex digits), never the commons. Held only by a supervised service whose service.json says \"agentMemory\": true.",
    ),
    scope(
        SALIENCE_WRITE_OBSERVED,
        "salience",
        "write",
        "Write the valuations of the agent a call names as its observer, never the commons. Held only by a supervised service whose service.json says \"agentMemory\": true.",
    ),
];

/// The two spellings of the observer the Geist tools read, in the order they
/// read them (`mcp_arg_string`: the first non-empty string wins).
pub(crate) const OBSERVER_ARGUMENT_KEYS: &[&str] = &["observer_agent_id", "observerAgentId"];

/// The scopes of a supervised service's own MCP token: the member grant, and
/// the observed writes when its manifest asks for agent memory.
pub(crate) fn service_token_scopes(agent_memory: bool) -> Vec<String> {
    let mut scopes = crate::loopback_token_grants::grant_profile_scopes(
        crate::loopback_token_grants::MEMBER_GRANT_PROFILE,
    );
    if agent_memory {
        scopes.extend(
            AGENT_MEMORY_SCOPES
                .iter()
                .map(|detail| detail.key().to_string()),
        );
    }
    scopes
}

/// An agent id as gardend names witness graphs by it: `agent-` and 16
/// lowercase hex digits (Choreograph's `mintStandaloneAgentId`). An IRI
/// (`urn:sophia:agent:agent-…`) names a different graph, and a person's name
/// names no agent.
pub(crate) fn is_agent_id(value: &str) -> bool {
    value.strip_prefix("agent-").is_some_and(|hex| {
        hex.len() == 16
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// The observer an argument object names: both spellings must agree, since
/// not every tool reads them in the same order (`value` after source
/// activation reads `observerAgentId` first).
fn named_observer(arguments: &serde_json::Value) -> Result<Option<String>, String> {
    let mut spellings = OBSERVER_ARGUMENT_KEYS
        .iter()
        .filter_map(|key| crate::mcp_utils::mcp_arg_string(arguments, &[key]));
    let first = spellings.next();
    if let Some(other) = spellings.find(|other| Some(other) != first.as_ref()) {
        return Err(format!(
            "observer_agent_id {first:?} and observerAgentId {other:?} name different observers"
        ));
    }
    Ok(first)
}

/// The agent a witness-bound call writes for, read as the tool reads it, or
/// why the call names none. For `remember_batch` the batch names it, and
/// every record names none or the same one (the tool files each record for
/// the batch's observer).
pub(crate) fn observed_call_agent(
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<String, String> {
    let named = named_observer(arguments)?
        .ok_or_else(|| "the call names no observer_agent_id".to_string())?;
    if !is_agent_id(&named) {
        return Err(format!(
            "observer_agent_id {named:?} is not an agent id (agent- and 16 lowercase hex digits)"
        ));
    }
    if tool == "remember_batch" {
        let records = arguments
            .get("records")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (index, record) in records.iter().enumerate() {
            let own = named_observer(record).map_err(|why| format!("records[{index}]: {why}"))?;
            if let Some(own) = own {
                if own != named {
                    return Err(format!(
                        "records[{index}] names observer {own}, not the batch's {named}"
                    ));
                }
            }
        }
    }
    Ok(named)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_agent_id_is_agent_and_sixteen_lowercase_hex_digits() {
        assert!(is_agent_id("agent-0123456789abcdef"));
        for not_one in [
            "",
            "agent-",
            "agent-0123456789abcde",
            "agent-0123456789abcdef0",
            "agent-0123456789ABCDEF",
            "agent-0123456789abcdeg",
            "urn:sophia:agent:agent-0123456789abcdef",
            "vera",
            " agent-0123456789abcdef",
        ] {
            assert!(!is_agent_id(not_one), "{not_one:?}");
        }
    }

    /// The gate reads the observer exactly as the tools do: the first
    /// non-empty spelling, trimmed.
    #[test]
    fn an_observed_call_names_its_agent_as_the_tool_reads_it() {
        let agent = "agent-0123456789abcdef";
        assert_eq!(
            observed_call_agent("remember", &json!({"observer_agent_id": agent})).as_deref(),
            Ok(agent)
        );
        assert_eq!(
            observed_call_agent("sing", &json!({"observerAgentId": format!(" {agent} ")}))
                .as_deref(),
            Ok(agent)
        );
        // An empty first spelling is skipped by the tools too.
        assert_eq!(
            observed_call_agent(
                "value",
                &json!({"observer_agent_id": "  ", "observerAgentId": agent})
            )
            .as_deref(),
            Ok(agent)
        );
        // Two spellings must name one observer: the tools do not all read
        // them in the same order.
        assert!(observed_call_agent(
            "remember",
            &json!({"observer_agent_id": "vera", "observerAgentId": agent})
        )
        .is_err());
        assert!(observed_call_agent(
            "value",
            &json!({"observer_agent_id": agent, "observerAgentId": "agent-fedcba9876543210"})
        )
        .is_err_and(|why| why.contains("different observers")));
        assert_eq!(
            observed_call_agent(
                "value",
                &json!({"observer_agent_id": agent, "observerAgentId": agent})
            )
            .as_deref(),
            Ok(agent)
        );
        assert!(observed_call_agent("remember", &json!({})).is_err());
        assert!(observed_call_agent("remember", &json!({"observer_agent_id": 7})).is_err());
        let batch = |records: serde_json::Value| {
            observed_call_agent(
                "remember_batch",
                &json!({"observer_agent_id": agent, "records": records}),
            )
        };
        assert!(batch(json!([{"content": "x"}, {"observerAgentId": agent}])).is_ok());
        assert!(
            batch(json!([{"observer_agent_id": "agent-fedcba9876543210"}]))
                .is_err_and(|why| why.contains("records[0]"))
        );
    }
}

/// End to end: the real router's `/mcp`, a real supervised child process (the
/// test binary re-run as a service, `local_service_host::child`) and the real
/// memory, Song and valuation stores. Named so the cells lane's gating filter
/// (`local_service_host`) runs them.
#[cfg(all(test, feature = "headless"))]
mod local_service_host_agent_memory_tests {
    use crate::cell_graph_boundary::CellGraphBoundary;
    use crate::local_service_host::child::{
        fast_timing, get_json, wait_service_healthy, write_service_manifest,
    };
    use crate::local_service_host::LocalServiceHost;
    use crate::loopback_client_token_service::create_client_token;
    use crate::loopback_client_token_types::CreateLoopbackClientTokenInput;
    use crate::loopback_self_host::SelfHost;
    use crate::loopback_state::{LoopbackManifest, LoopbackState};
    use axum::body::Body;
    use serde_json::{json, Value};
    use std::path::Path;
    use std::sync::Arc;
    use tower::ServiceExt;

    const MASTER: &str = "agent-memory-test-master";
    const GRAPH: &str = "being-lab";
    const BEING: &str = "agent-0123456789abcdef";
    const OTHER_BEING: &str = "agent-fedcba9876543210";
    /// Declares `"agentMemory": true`.
    const COMPUTE: &str = "compute";
    /// Declares nothing.
    const PLAIN: &str = "plain";

    fn state(app: crate::app_runtime::AppHandle, dir: &Path) -> Arc<LoopbackState> {
        let grant = crate::loopback_token_grants::session_all_token_grant();
        Arc::new(LoopbackState {
            app,
            token: MASTER.into(),
            manifest: LoopbackManifest {
                runtime_profile: "test",
                bind_host: "127.0.0.1",
                port: 0,
                api_url: "https://sophia.example.com".into(),
                mcp_url: "https://sophia.example.com/mcp".into(),
                openapi_url: String::new(),
                token: MASTER.into(),
                pid: 0,
                started_at: String::new(),
                manifest_path: String::new(),
                auth_header: "Authorization: Bearer <token>",
                token_audience: "test",
                token_storage: "memory",
                security_warning: "synthetic test manifest",
                cell_graph_id: None,
                cell_owner: None,
                cell_generation: None,
                cell_registry_revision: None,
                capabilities: grant.scopes.clone(),
                token_scope_mode: grant.scope_mode,
                token_scopes: grant.scopes,
                scope_details: grant.scope_details,
                grant_profiles: grant.grant_profiles,
            },
            jobs: Arc::new(crate::local_jobs::LocalJobRegistry::new(dir.join("jobs")).unwrap()),
            services: Arc::new(LocalServiceHost::default()),
            lifecycle: Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
            cell_graph: Arc::new(CellGraphBoundary::for_test(None)),
            self_host: Arc::new(SelfHost::default()),
        })
    }

    /// Start `service_id` with the extra manifest fields `extra`, and return
    /// the MCP token gardend handed it (read from the child's environment).
    fn start_service(
        state: &Arc<LoopbackState>,
        root: &Path,
        service_id: &str,
        extra: Value,
    ) -> String {
        state.services.pin_manifest(
            service_id,
            write_service_manifest(&root.join(service_id), service_id, &[], extra),
        );
        state.services.set_timing(fast_timing());
        state
            .services
            .set_local_api_url("http://127.0.0.1:9".into());
        state
            .services
            .start(service_id, &state.app, &state.manifest)
            .unwrap();
        let target = wait_service_healthy(&state.services, service_id);
        let echoed = get_json(&format!("{}/env", target.base_url)).expect("the child's env");
        echoed["env"]["MNEMOSYNE_MCP_ACCESS_TOKEN"]
            .as_str()
            .expect("the service's token")
            .to_string()
    }

    fn with_profile(name: &str, test: impl FnOnce(&Path)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!(
            "garden-agent-memory-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        let profile = root.join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&root)));
        match previous {
            Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&root);
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }

    async fn rpc(state: &Arc<LoopbackState>, token: &str, method: &str, params: Value) -> Value {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
            ))
            .unwrap();
        let response = crate::loopback_router::loopback_router(state.clone())
            .oneshot(request)
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 50_000_000)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn call(state: &Arc<LoopbackState>, token: &str, tool: &str, arguments: Value) -> Value {
        rpc(
            state,
            token,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        )
        .await
    }

    fn remember_arguments(observer: Option<&str>, content: &str) -> Value {
        let mut arguments = json!({"graphId": GRAPH, "content": content, "block_ids": ["b1"]});
        if let Some(observer) = observer {
            arguments["observer_agent_id"] = json!(observer);
        }
        arguments
    }

    /// The answer is a -32003 that names the full scope the call lacks.
    fn assert_refused(answer: &Value, scope: &str) {
        assert_eq!(answer["error"]["code"], -32003, "{answer}");
        let message = answer["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.starts_with(&format!("loopback token missing required scope {scope}")),
            "{answer}"
        );
    }

    fn recalled(answer: &Value) -> Vec<String> {
        answer["result"]["structuredContent"]["memories"]
            .as_array()
            .unwrap_or_else(|| panic!("{answer}"))
            .iter()
            .map(|memory| memory["content"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// A service whose `service.json` says `"agentMemory": true` may write the
    /// memory, Song and valuations of the agent a call names as its observer,
    /// and nothing of the commons. A service that does not say so, and a
    /// person's member token, may not write memory at all.
    #[test]
    fn a_service_with_agent_memory_writes_only_witnessed_memory() {
        with_profile("grant", |root| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            crate::graph_service::create_graph_service(
                &app,
                crate::graph_service::CreateGraphInput {
                    graph_id: Some(GRAPH.to_string()),
                    title: "Being lab".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .unwrap();
            let state = state(app, root);
            let compute = start_service(&state, root, COMPUTE, json!({"agentMemory": true}));
            let plain = start_service(&state, root, PLAIN, json!({}));
            let member = create_client_token(
                &state.app,
                CreateLoopbackClientTokenInput {
                    label: Some("a person".into()),
                    grant_profile_id: Some("member".into()),
                    scopes: None,
                    expires_in_days: None,
                },
            )
            .unwrap()
            .token;

            crate::app_runtime::async_runtime::block_on(async {
                // Remembering for a being: allowed, and only for one named the
                // way gardend names witness graphs (agent- and 16 hex digits).
                let stored = call(
                    &state,
                    &compute,
                    "remember",
                    remember_arguments(Some(BEING), "the source repeats itself"),
                )
                .await;
                assert_eq!(
                    stored["result"]["structuredContent"]["ok"], true,
                    "{stored}"
                );
                assert!(
                    stored["result"]["structuredContent"]["subject"]
                        .as_str()
                        .is_some_and(|s| s.contains(&format!(":projection:memory:agent:{BEING}:"))),
                    "{stored}"
                );
                for observer in [
                    None,
                    Some("vera"),
                    Some("urn:sophia:agent:agent-0123456789abcdef"),
                    Some("agent-0123456789ABCDEF"),
                    Some("agent-0123456789abcde"),
                ] {
                    assert_refused(
                        &call(
                            &state,
                            &compute,
                            "remember",
                            remember_arguments(observer, "x"),
                        )
                        .await,
                        "memory.write",
                    );
                }
                // Nor may a service that did not ask, nor a person's member token.
                for token in [&plain, &member] {
                    assert_refused(
                        &call(
                            &state,
                            token,
                            "remember",
                            remember_arguments(Some(BEING), "x"),
                        )
                        .await,
                        "memory.write",
                    );
                }

                // What the being remembered is its own: not the commons', not another being's.
                let own = call(
                    &state,
                    &compute,
                    "recall",
                    json!({"graphId": GRAPH, "observer_agent_id": BEING}),
                )
                .await;
                assert_eq!(recalled(&own), vec!["the source repeats itself"]);
                let commons = call(&state, &member, "recall", json!({"graphId": GRAPH})).await;
                assert_eq!(recalled(&commons), Vec::<String>::new());
                let other = call(
                    &state,
                    &compute,
                    "recall",
                    json!({"graphId": GRAPH, "observerAgentId": OTHER_BEING}),
                )
                .await;
                assert_eq!(recalled(&other), Vec::<String>::new());

                // A batch is one being's act.
                let forged = call(
                    &state,
                    &compute,
                    "remember_batch",
                    json!({"graphId": GRAPH, "observer_agent_id": BEING, "records": [
                        {"content": "mine", "block_ids": ["b1"]},
                        {"content": "theirs", "block_ids": ["b1"], "observer_agent_id": OTHER_BEING},
                    ]}),
                )
                .await;
                assert!(forged.get("error").is_some(), "{forged}");
                let batch = call(
                    &state,
                    &compute,
                    "remember_batch",
                    json!({"graphId": GRAPH, "observer_agent_id": BEING, "records": [
                        {"content": "a second note", "block_ids": ["b1"]},
                    ]}),
                )
                .await;
                assert_eq!(batch["result"]["structuredContent"]["ok"], true, "{batch}");

                // The Song: a being that never sang has none; it sings its own.
                let silent = call(
                    &state,
                    &compute,
                    "music",
                    json!({"graphId": GRAPH, "observer_agent_id": BEING}),
                )
                .await;
                assert_eq!(
                    silent["result"]["structuredContent"]["verses"],
                    json!([]),
                    "{silent}"
                );
                let sung = call(
                    &state,
                    &compute,
                    "sing",
                    json!({"graphId": GRAPH, "observer_agent_id": BEING, "verse": "I read slowly."}),
                )
                .await;
                assert!(sung.get("result").is_some(), "{sung}");
                assert_refused(
                    &call(
                        &state,
                        &compute,
                        "sing",
                        json!({"graphId": GRAPH, "verse": "commons"}),
                    )
                    .await,
                    "memory.write",
                );

                // Valuations: the same narrow grant.
                let valued = call(
                    &state,
                    &compute,
                    "value",
                    json!({"graphId": GRAPH, "observer_agent_id": BEING,
                           "document_id": "d1", "block_id": "b1", "importance": 4}),
                )
                .await;
                assert!(valued.get("result").is_some(), "{valued}");
                let revalued = call(
                    &state,
                    &compute,
                    "revaluate",
                    json!({"graphId": GRAPH, "observer_agent_id": BEING, "weights": "importance_weight: 0.5"}),
                )
                .await;
                assert!(revalued.get("result").is_some(), "{revalued}");
                assert_refused(
                    &call(
                        &state,
                        &compute,
                        "value",
                        json!({"graphId": GRAPH, "document_id": "d1", "block_id": "b1", "importance": 4}),
                    )
                    .await,
                    "salience.write",
                );
                assert_refused(
                    &call(
                        &state,
                        &plain,
                        "value",
                        json!({"graphId": GRAPH, "observer_agent_id": BEING,
                               "document_id": "d1", "block_id": "b1", "importance": 4}),
                    )
                    .await,
                    "salience.write",
                );

                // The commons queue's own tools are not part of the grant.
                for tool in ["care", "archive_memories"] {
                    assert_refused(
                        &call(
                            &state,
                            &compute,
                            tool,
                            json!({"graphId": GRAPH, "observer_agent_id": BEING}),
                        )
                        .await,
                        "memory.write",
                    );
                }

                // Compute can be told what it may do: each listed tool says
                // whether this token may call it, and a witness tool names the
                // scope that admits it for an observer.
                let callable = |listed: &Value, name: &str| {
                    listed["result"]["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|tool| tool["name"] == name)
                        .unwrap_or_else(|| panic!("{name} is listed"))["_meta"]
                        .clone()
                };
                let listed = rpc(&state, &compute, "tools/list", json!({})).await;
                let remember = callable(&listed, "remember");
                assert_eq!(
                    remember["sophia.local.callable"], "with-agent-observer",
                    "{remember}"
                );
                assert_eq!(
                    remember["sophia.local.observedScopes"],
                    json!({"memory.write": "memory.write.observed"})
                );
                assert_eq!(
                    callable(&listed, "value")["sophia.local.callable"],
                    "with-agent-observer"
                );
                assert_eq!(callable(&listed, "recall")["sophia.local.callable"], true);
                assert_eq!(callable(&listed, "care")["sophia.local.callable"], false);
                assert_eq!(
                    callable(&listed, "rdf_load")["sophia.local.callable"],
                    false
                );
                let listed = rpc(&state, &plain, "tools/list", json!({})).await;
                assert_eq!(
                    callable(&listed, "remember")["sophia.local.callable"],
                    false
                );
                assert_eq!(callable(&listed, "recall")["sophia.local.callable"], true);
                let listed = rpc(&state, MASTER, "tools/list", json!({})).await;
                assert_eq!(callable(&listed, "remember")["sophia.local.callable"], true);
            });

            for service_id in [COMPUTE, PLAIN] {
                state.services.stop(service_id, &state.app).unwrap();
            }
        });
    }

    /// Only a supervised service's own token can carry the observed scopes:
    /// no grant profile (the master key's `session-all` included) holds them,
    /// and no client token can be minted with them.
    #[test]
    fn only_a_service_token_carries_the_observed_scopes() {
        with_profile("mint", |_root| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let grant = crate::loopback_token_grants::session_all_token_grant();
            for scope in ["memory.write.observed", "salience.write.observed"] {
                assert!(!grant.scopes.contains(&scope), "{scope}");
                for profile in &grant.grant_profiles {
                    assert!(!profile.scopes.contains(&scope), "{} {scope}", profile.id);
                }
                let minted = create_client_token(
                    &app,
                    CreateLoopbackClientTokenInput {
                        label: Some("forged".into()),
                        grant_profile_id: None,
                        scopes: Some(vec!["mcp.tools.call".into(), scope.into()]),
                        expires_in_days: None,
                    },
                );
                assert!(minted.is_err(), "{scope}");
            }
        });
    }
}
