use crate::loopback_scopes::{
    crdt_operation_scope_rules, mcp_tool_required_scope_mode, mcp_tool_scopes, TOKEN_SCOPE_MODE,
};
use std::{collections::HashSet, sync::OnceLock};

/// True iff `name` is a registered MCP tool in the embedded catalog
/// (`mcp_tool_catalog.json`) — the closed set of tool identifiers this cell
/// actually exposes. Used by the Observatory Capture spine's `ToolName`
/// (`capture_event.rs`) so an aggregate `tool_name` field can only ever hold
/// a real tool name, never identifier-shaped free text (e.g. a diagnosis or
/// a person's name that happens to pass a permissive identifier regex).
///
/// Deliberately does NOT reuse `mcp_tools_list_result` — that re-parses the
/// full catalog (JSON Schemas, scope metadata) on every call, which would be
/// wasteful for what should be a cheap per-event membership check. This
/// parses the catalog once into a name-only set and caches it.
pub(super) fn is_known_mcp_tool_name(name: &str) -> bool {
    static NAMES: OnceLock<HashSet<String>> = OnceLock::new();
    NAMES
        .get_or_init(|| {
            let catalog: serde_json::Value =
                serde_json::from_str(include_str!("mcp_tool_catalog.json"))
                    .expect("embedded MCP tool catalog must be valid JSON");
            catalog["tools"]
                .as_array()
                .expect("catalog tools array")
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .contains(name)
}

/// The MCP `tools/list` result is intentionally a typed envelope that hands
/// the embedded catalog tools straight through. Each tool entry's `inputSchema`
/// is JSON Schema authored in `mcp_tool_catalog.json` (free-form by design),
/// and `_meta` keys carry dots (e.g. `sophia.local.requiredScopes`) which are
/// not valid Rust identifiers — so the inner tool shapes remain
/// `serde_json::Value`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct McpToolsListResult {
    /// JSON-Schema tool descriptors loaded from `mcp_tool_catalog.json`. Kept
    /// as `serde_json::Value` because each `inputSchema` is free-form JSON
    /// Schema and `_meta` keys carry dots, which Rust struct fields cannot.
    tools: Vec<serde_json::Value>,
}

pub(super) fn mcp_tools_list_result() -> McpToolsListResult {
    let mut envelope: McpToolsListResult =
        serde_json::from_str(include_str!("mcp_tool_catalog.json"))
            .expect("embedded MCP tool catalog must be valid JSON");
    attach_mcp_scope_metadata(&mut envelope.tools);
    envelope
}

pub(super) fn mcp_tools_list_result_for_boundary(
    boundary: &crate::cell_graph_boundary::CellGraphBoundary,
    role: crate::cell_graph_boundary::CellRole,
) -> McpToolsListResult {
    mcp_tools_list_result_with_optional(boundary, role, false)
}

pub(super) fn mcp_tools_list_result_with_optional(
    boundary: &crate::cell_graph_boundary::CellGraphBoundary,
    role: crate::cell_graph_boundary::CellRole,
    custom_css: bool,
) -> McpToolsListResult {
    let mut envelope = mcp_tools_list_result();
    envelope.tools.retain(|tool| {
        (custom_css || tool.get("optionalCapability").is_none()) &&
        tool.get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| boundary.mcp_tool_visible_for_role(name, role))
    });
    envelope
}

fn attach_mcp_scope_metadata(tools: &mut [serde_json::Value]) {
    for tool in tools {
        let Some(name) = tool
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let scopes = mcp_tool_scopes(&name);
        if scopes.is_empty() {
            continue;
        }
        let required_scope_mode = mcp_tool_required_scope_mode(&name);
        let is_crdt_operation = name == "crdt_operation";
        if let Some(tool_object) = tool.as_object_mut() {
            let mut meta = serde_json::json!({
                "sophia.local.requiredScopes": scopes,
                "sophia.local.scopeMode": TOKEN_SCOPE_MODE,
                "sophia.local.requiredScopeMode": required_scope_mode,
            });
            if is_crdt_operation {
                meta["sophia.local.operationScopes"] = crdt_operation_scope_rules()
                    .iter()
                    .map(|rule| (rule.kind.to_string(), serde_json::json!(rule.scopes)))
                    .collect::<serde_json::Map<String, serde_json::Value>>()
                    .into();
            }
            tool_object.insert("_meta".to_string(), meta);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopback_scope_catalog::loopback_scope_keys;
    use std::collections::BTreeSet;

    #[test]
    fn tool_registry_names_are_unique_and_schema_shaped() {
        let result = serde_json::to_value(mcp_tools_list_result())
            .expect("McpToolsListResult always serializes");
        let tools = result
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .expect("tools array");
        // 102 base + 2 flow-board tools (main) + 8 engine-line tools
        // (custom_css/read_custom_css/write_custom_css, agent_status,
        // status, create_document_once, create_artifact_text,
        // write_artifact_text) = 112.
        assert_eq!(tools.len(), 113);

        let known_scopes = loopback_scope_keys()
            .into_iter()
            .collect::<BTreeSet<&'static str>>();
        let mut names = BTreeSet::new();
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .expect("tool name");
            assert!(names.insert(name.to_string()), "duplicate tool name {name}");
            assert!(
                tool.get("description")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|description| !description.trim().is_empty()),
                "tool {name} must have a non-empty description"
            );
            assert_eq!(
                tool.pointer("/inputSchema/type")
                    .and_then(serde_json::Value::as_str),
                Some("object"),
                "tool {name} must declare an object input schema"
            );
            let scopes = tool
                .pointer("/_meta/sophia.local.requiredScopes")
                .and_then(serde_json::Value::as_array)
                .expect("scope metadata");
            assert!(!scopes.is_empty(), "tool {name} must declare local scopes");
            for scope in scopes {
                let scope = scope.as_str().expect("scope string");
                assert!(
                    known_scopes.contains(scope),
                    "tool {name} declared unknown scope {scope}"
                );
            }
            assert_eq!(
                tool.pointer("/_meta/sophia.local.scopeMode")
                    .and_then(serde_json::Value::as_str),
                Some(TOKEN_SCOPE_MODE),
                "tool {name} must declare scope mode"
            );
            assert_eq!(
                tool.pointer("/_meta/sophia.local.requiredScopeMode")
                    .and_then(serde_json::Value::as_str),
                Some(mcp_tool_required_scope_mode(name)),
                "tool {name} must declare required scope mode"
            );
            if name == "crdt_operation" {
                let operation_scopes = tool
                    .pointer("/_meta/sophia.local.operationScopes")
                    .and_then(serde_json::Value::as_object)
                    .expect("crdt_operation operation scopes");
                assert!(
                    operation_scopes.contains_key("document.write"),
                    "crdt_operation must expose document operation scopes"
                );
                for (operation_kind, scopes) in operation_scopes {
                    let scopes = scopes.as_array().unwrap_or_else(|| {
                        panic!("operation {operation_kind} scopes must be an array")
                    });
                    assert!(
                        !scopes.is_empty(),
                        "operation {operation_kind} must have scopes"
                    );
                    for scope in scopes {
                        let scope = scope.as_str().expect("operation scope string");
                        assert!(
                            known_scopes.contains(scope),
                            "operation {operation_kind} declared unknown scope {scope}"
                        );
                    }
                }
            }
        }
        assert!(names.contains("list_graphs"));
        assert!(names.contains("write_document"));
        assert!(names.contains("create_document_once"));
        assert!(names.contains("semantic_search"));
        assert!(names.contains("memory_semantic_recall"));
        assert!(names.contains("semantic_reason"));
        assert!(names.contains("graph_intuition"));
        assert!(names.contains("workflow_book_open"));
        assert!(names.contains("workflow_book_choose"));
        assert!(names.contains("workflow_book_apply"));
        assert!(names.contains("workflow_book_compose"));
        assert!(names.contains("workflow_book_validate"));
        assert!(names.contains("workflow_authoring_session"));
        assert!(names.contains("workflow_run_start"));
        assert!(names.contains("workflow_run_monitor"));
        assert!(names.contains("source_pull"));
        assert!(names.contains("source_push"));
        assert!(names.contains("source_rebuild"));
    }

    #[test]
    fn is_known_mcp_tool_name_is_a_closed_allowlist() {
        assert!(is_known_mcp_tool_name("sparql_query"));
        assert!(is_known_mcp_tool_name("read_document"));
        assert!(is_known_mcp_tool_name("write_document"));
        assert!(is_known_mcp_tool_name("create_document_once"));
        // Identifier-shaped, but not a registered tool — must be rejected,
        // not merely "shape valid".
        assert!(!is_known_mcp_tool_name("MyPrivateDiagnosis"));
        assert!(!is_known_mcp_tool_name("sparql_query_but_typo"));
        assert!(!is_known_mcp_tool_name(""));
    }

    #[test]
    fn desktop_catalog_exposes_the_complete_agent_tool_surface() {
        let boundary = crate::cell_graph_boundary::CellGraphBoundary::for_test(None);
        let result = serde_json::to_value(mcp_tools_list_result_for_boundary(
            &boundary,
            crate::cell_graph_boundary::CellRole::Owner,
        ))
        .expect("desktop tools list serializes");
        let tools = result["tools"].as_array().expect("tools");

        // 113 registered tools minus the two explicitly optional CSS schemas.
        assert_eq!(tools.len(), 111);
        assert!(tools.iter().any(|tool| tool["name"] == "read_document"));
        assert!(tools.iter().any(|tool| tool["name"] == "write_document"));
        assert!(tools.iter().any(|tool| tool["name"] == "create_document_once"));
    }

    #[test]
    fn cell_catalog_omits_profile_lifecycle_host_file_and_service_effect_tools() {
        let boundary = crate::cell_graph_boundary::CellGraphBoundary::for_test(Some("graph-a"));
        let result = serde_json::to_value(mcp_tools_list_result_for_boundary(
            &boundary,
            crate::cell_graph_boundary::CellRole::Owner,
        ))
        .expect("cell tools list serializes");
        let names = result["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<BTreeSet<_>>();

        // 113 catalog tools − 9 profile-denied − 2 optional CSS schemas
        // (opt-in, not default-discoverable) = 102. G4's two flow tools are
        // cell-visible (documents.write.crdt / documents.read). The three
        // source_* tools are cell-visible too: main's 0db7586 originally
        // fenced them (mcpProfileDenied) the day the registry gained them
        // unclassified, but 6e70030 "fix(cell): authorize graph source
        // tools for editors" (2026-09-06, upstream of the deployed
        // fix/cutover-history-sharing-20260920 engine line) superseded that
        // with real Editor+ role mapping (sources.read/write/rebuild are
        // classified Editor in `minimum_role_for_effect`) — the merge keeps
        // that later, deployed decision, not main's earlier blanket fence.
        assert_eq!(names.len(), 102);
        for added in ["custom_css", "agent_status", "agent_self_image", "status"] {
            assert!(names.contains(added), "missing default tool {added}");
        }
        for optional in ["read_custom_css", "write_custom_css"] {
            assert!(!names.contains(optional), "optional tool leaked into default discovery: {optional}");
        }
        for denied in [
            "create_graph",
            "duplicate_graph",
            "graph_intuition",
            "list_graphs",
            "manage_graph",
            "upload_artifact",
            "workflow_authoring_session",
            "workflow_run_monitor",
            "workflow_run_start",
        ] {
            assert!(!names.contains(denied));
        }
        assert!(names.contains("read_document"));
        assert!(names.contains("get_job_status"));
        assert!(names.contains("create_document_once"));
        assert!(names.contains("source_pull"));
        assert!(names.contains("source_push"));
        assert!(names.contains("source_rebuild"));
    }
}
