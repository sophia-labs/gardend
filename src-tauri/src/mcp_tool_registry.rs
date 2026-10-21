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
        (custom_css || tool.get("optionalCapability").is_none())
            && tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| boundary.mcp_tool_visible_for_role(name, role))
    });
    envelope
}

/// `tools/list` for a caller bound to one graph (a delegation token): the
/// tools it may call there (the bindable policy classes, and only those whose
/// scopes it holds), with the graph argument taken out of every schema, since
/// gardend fills it in.
pub(super) fn mcp_tools_list_result_for_bound_graph(
    boundary: &crate::cell_graph_boundary::CellGraphBoundary,
    token_scopes: &[String],
    custom_css: bool,
) -> McpToolsListResult {
    let held = |scope: &str| token_scopes.iter().any(|candidate| candidate == scope);
    let mut envelope = mcp_tools_list_result();
    envelope.tools.retain(|tool| {
        let Some(name) = tool.get("name").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let scopes = mcp_tool_scopes(name);
        let callable = if mcp_tool_required_scope_mode(name) == "operation-kind" {
            scopes.iter().any(|scope| held(scope))
        } else {
            scopes.iter().all(|scope| held(scope))
        };
        (custom_css || tool.get("optionalCapability").is_none())
            && boundary.mcp_tool_bindable(name)
            && callable
    });
    for tool in &mut envelope.tools {
        if let Some(schema) = tool.get_mut("inputSchema") {
            strip_graph_argument(schema);
        }
    }
    envelope
}

/// Take the graph argument out of a JSON schema: its properties, its
/// `required` lists, and the `anyOf`/`oneOf`/`allOf` branches that only asked
/// for it (`write_document` requires `graph_id` or `graphId` through one).
fn strip_graph_argument(schema: &mut serde_json::Value) {
    const GRAPH_ARGUMENTS: [&str; 2] = ["graphId", "graph_id"];
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if let Some(properties) = object
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        for name in GRAPH_ARGUMENTS {
            properties.remove(name);
        }
    }
    if let Some(required) = object
        .get_mut("required")
        .and_then(serde_json::Value::as_array_mut)
    {
        required.retain(|name| {
            !name
                .as_str()
                .is_some_and(|name| GRAPH_ARGUMENTS.contains(&name))
        });
    }
    // A branch left asking for nothing: an `anyOf`/`oneOf` holding one is
    // met by anything (drop it whole); in an `allOf` it is just dropped.
    let asks_nothing = |branch: &serde_json::Value| {
        branch.as_object().is_some_and(|branch| {
            branch.iter().all(|(key, value)| {
                key == "required" && value.as_array().is_some_and(|names| names.is_empty())
            })
        })
    };
    for key in ["anyOf", "oneOf", "allOf"] {
        let Some(branches) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for branch in branches.iter_mut() {
            strip_graph_argument(branch);
        }
        if key == "allOf" {
            branches.retain(|branch| !asks_nothing(branch));
        } else if branches.iter().any(asks_nothing) {
            branches.clear();
        }
        if branches.is_empty() {
            object.remove(key);
        }
    }
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
        // write_artifact_text) = 112 (113 with the one added since), then
        // + datasheet_import and datasheet_facts (datasheets slice 1) = 115,
        // + find (the RDF entity index) = 116, + the three RDF-native
        // workflow plan tools (workflow_plan_create/read/compose) = 119,
        // + workflow_plan_publish = 120, + the scan reader's two
        // (read_scan_text, scan_text_layer) = 122, + workflow_run_record = 123,
        // + emporium_describe (typed objects) = 124.
        assert_eq!(tools.len(), 124);

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

        // 124 registered tools minus the two explicitly optional CSS schemas.
        assert_eq!(tools.len(), 122);
        assert!(tools.iter().any(|tool| tool["name"] == "read_document"));
        assert!(tools.iter().any(|tool| tool["name"] == "write_document"));
        assert!(tools
            .iter()
            .any(|tool| tool["name"] == "create_document_once"));
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

        // 115 catalog tools − 9 profile-denied − 2 optional CSS schemas
        // (opt-in, not default-discoverable) = 104. G4's two flow tools are
        // cell-visible (documents.write.crdt / documents.read). The three
        // source_* tools are cell-visible too: main's 0db7586 originally
        // fenced them (mcpProfileDenied) the day the registry gained them
        // unclassified, but 6e70030 "fix(cell): authorize graph source
        // tools for editors" (2026-09-06, upstream of the deployed
        // fix/cutover-history-sharing-20260920 engine line) superseded that
        // with real Editor+ role mapping (sources.read/write/rebuild are
        // classified Editor in `minimum_role_for_effect`) — the merge keeps
        // that later, deployed decision, not main's earlier blanket fence.
        // + the four workflow plan tools (graph-scoped, cell-visible) = 108,
        // + read_scan_text and scan_text_layer (graph-scoped) = 110,
        // + workflow_run_record (graph-scoped) = 111, + emporium_describe
        // (graph-scoped; emporium_vocab moved from global to graph-scoped and
        // stays visible) = 112.
        assert_eq!(names.len(), 112);
        for added in ["custom_css", "agent_status", "agent_self_image", "status"] {
            assert!(names.contains(added), "missing default tool {added}");
        }
        for optional in ["read_custom_css", "write_custom_css"] {
            assert!(
                !names.contains(optional),
                "optional tool leaked into default discovery: {optional}"
            );
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

    /// A delegation token's `tools/list`: what it may call on its graph, and
    /// no graph argument anywhere (gardend fills it in).
    #[test]
    fn a_graph_bound_tool_list_has_no_graph_argument_and_only_callable_tools() {
        let boundary = crate::cell_graph_boundary::CellGraphBoundary::for_test(None);
        let member = crate::loopback_token_grants::grant_profile_scopes("member");
        let mut viewer = crate::loopback_token_grants::grant_profile_scopes("read-only");
        viewer.push("mcp.tools.call".into());
        let names_of = |scopes: &[String]| {
            let listed = serde_json::to_value(mcp_tools_list_result_for_bound_graph(
                &boundary, scopes, false,
            ))
            .unwrap();
            for tool in listed["tools"].as_array().unwrap() {
                let schema = tool["inputSchema"].to_string();
                assert!(
                    !schema.contains("\"graphId\"") && !schema.contains("\"graph_id\""),
                    "{}: {schema}",
                    tool["name"]
                );
            }
            listed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_string())
                .collect::<BTreeSet<_>>()
        };
        let writer = names_of(&member);
        let reader = names_of(&viewer);
        for tool in [
            "read_document",
            "search_blocks",
            "datasheet_facts",
            "read_artifact",
        ] {
            assert!(writer.contains(tool) && reader.contains(tool), "{tool}");
        }
        for tool in [
            "write_document",
            "insert_blocks",
            "edit_comment",
            "create_wires",
            "create_artifact_text",
        ] {
            assert!(writer.contains(tool), "a member writes with {tool}");
            assert!(!reader.contains(tool), "a viewer is not offered {tool}");
        }
        for tool in [
            "list_graphs",
            "create_graph",
            "upload_artifact",
            "sparql_update",
            "rdf_load",
        ] {
            assert!(
                !writer.contains(tool),
                "{tool} is not for a graph-bound member"
            );
        }
        let write_document = serde_json::to_value(mcp_tools_list_result_for_bound_graph(
            &boundary, &member, false,
        ))
        .unwrap()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "write_document")
            .cloned()
            .unwrap();
        // Flat since 2026-10-14 (OpenAI refuses a top-level allOf); the
        // document and content requirements are the handler's, and said in
        // the description.
        assert!(write_document["inputSchema"].get("allOf").is_none());
        assert!(write_document["inputSchema"]
            .to_string()
            .contains("documentId"));
        assert!(write_document["description"]
            .as_str()
            .unwrap()
            .contains("exactly one of content or tiptapJson"));
    }

    /// Every input schema a tools/list can hand out must be one OpenAI's
    /// function calling takes (Anthropic's and Gemini's take the same shape):
    /// a plain `type: "object"` at the top, with no `allOf`/`anyOf`/`oneOf`/
    /// `enum`/`const`/`not` (or `if`/`then`/`else`) there (OpenAI's words:
    /// "schema must have type 'object' and not have 'oneOf'/'anyOf'/'allOf'/
    /// 'enum'/'const'/'not' at the top level"); and, all the way down, an
    /// array schema always says its `items`, as one schema, and nothing points
    /// through `$ref`. OpenAI refused gardend's `write_document` for its
    /// top-level `allOf`, so no owner's or editor's chat could run on an OpenAI
    /// model (V-followups-vehicle, 2026-10-13). Walks the whole catalog and
    /// every list a caller gets: desktop, cell, and graph-bound for each grant.
    #[test]
    fn every_tool_input_schema_has_the_openai_compatible_shape() {
        fn walk(tool: &str, path: &str, schema: &serde_json::Value, problems: &mut Vec<String>) {
            let Some(object) = schema.as_object() else {
                if !schema.is_boolean() {
                    problems.push(format!(
                        "{tool}{path}: a schema must be an object or a boolean"
                    ));
                }
                return;
            };
            if object.contains_key("$ref") {
                problems.push(format!("{tool}{path}: $ref"));
            }
            let types: Vec<&str> = match object.get("type") {
                Some(serde_json::Value::String(t)) => vec![t.as_str()],
                Some(serde_json::Value::Array(ts)) => {
                    ts.iter().filter_map(|t| t.as_str()).collect()
                }
                _ => Vec::new(),
            };
            if types.contains(&"array") {
                match object.get("items") {
                    Some(items) if items.is_object() || items.is_boolean() => {}
                    Some(_) => problems.push(format!("{tool}{path}: items must be one schema")),
                    None => problems.push(format!("{tool}{path}: array schema missing items")),
                }
            }
            if let Some(properties) = object.get("properties") {
                match properties.as_object() {
                    Some(properties) => {
                        for (name, property) in properties {
                            walk(
                                tool,
                                &format!("{path}/properties/{name}"),
                                property,
                                problems,
                            );
                        }
                    }
                    None => problems.push(format!("{tool}{path}: properties must be an object")),
                }
            }
            for key in ["items", "additionalProperties"] {
                if let Some(child) = object.get(key).filter(|child| child.is_object()) {
                    walk(tool, &format!("{path}/{key}"), child, problems);
                }
            }
            for key in ["anyOf", "oneOf", "allOf"] {
                for (index, branch) in object
                    .get(key)
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    walk(tool, &format!("{path}/{key}/{index}"), branch, problems);
                }
            }
        }

        fn audit(label: &str, listed: &McpToolsListResult) -> usize {
            let listed = serde_json::to_value(listed).unwrap();
            let tools = listed["tools"].as_array().expect("tools");
            let mut problems = Vec::new();
            for tool in tools {
                let name = tool["name"].as_str().unwrap_or("?");
                let schema = &tool["inputSchema"];
                let Some(top) = schema.as_object() else {
                    problems.push(format!("{name}: inputSchema must be an object"));
                    continue;
                };
                if top.get("type").and_then(serde_json::Value::as_str) != Some("object") {
                    problems.push(format!("{name}: top-level type must be \"object\""));
                }
                for key in [
                    "allOf", "anyOf", "oneOf", "enum", "const", "not", "if", "then", "else",
                ] {
                    if top.contains_key(key) {
                        problems.push(format!("{name}: top-level {key}"));
                    }
                }
                if let Some(required) = top.get("required") {
                    let declared = top.get("properties").and_then(serde_json::Value::as_object);
                    for field in required.as_array().into_iter().flatten() {
                        let field = field.as_str().unwrap_or("");
                        if !declared.is_some_and(|declared| declared.contains_key(field)) {
                            problems.push(format!("{name}: requires undeclared {field:?}"));
                        }
                    }
                }
                walk(name, "", schema, &mut problems);
            }
            assert!(problems.is_empty(), "{label}: {problems:#?}");
            tools.len()
        }

        assert_eq!(audit("catalog", &mcp_tools_list_result()), 124);
        let desktop = crate::cell_graph_boundary::CellGraphBoundary::for_test(None);
        let cell = crate::cell_graph_boundary::CellGraphBoundary::for_test(Some("graph-a"));
        for role in [
            crate::cell_graph_boundary::CellRole::Owner,
            crate::cell_graph_boundary::CellRole::Viewer,
        ] {
            assert!(
                audit(
                    "desktop",
                    &mcp_tools_list_result_with_optional(&desktop, role, true)
                ) > 0
            );
            assert!(
                audit(
                    "cell",
                    &mcp_tools_list_result_with_optional(&cell, role, true)
                ) > 0
            );
        }
        for grant in ["member", "read-only", "authoring", "session-all"] {
            let mut scopes = crate::loopback_token_grants::grant_profile_scopes(grant);
            scopes.push("mcp.tools.call".into());
            let listed = mcp_tools_list_result_for_bound_graph(&desktop, &scopes, true);
            assert!(audit(grant, &listed) > 0, "{grant}");
        }
    }
}
