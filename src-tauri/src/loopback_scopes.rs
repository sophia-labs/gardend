pub(crate) use crate::loopback_scope_catalog::LoopbackScopeDetail;
pub(crate) use crate::loopback_token_grants::TOKEN_SCOPE_MODE;
use serde::Deserialize;
use std::sync::OnceLock;

pub(crate) const STATIC_SCOPE_SELECTION_MODE: &str = "all";
const LOCAL_LOOPBACK_SURFACE_JSON: &str = include_str!("../../parity/local-loopback-surface.json");

static LOCAL_MCP_TOOL_SCOPE_ENTRIES: OnceLock<Vec<LocalMcpToolScopeEntry>> = OnceLock::new();

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalLoopbackSurface {
    mcp_tools: Vec<LocalMcpToolScopeEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalMcpToolScopeEntry {
    name: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    required_scope_mode: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CrdtOperationScopeRule {
    pub(crate) kind: &'static str,
    pub(crate) scopes: &'static [&'static str],
}

const CRDT_OPERATION_SCOPE_RULES: &[CrdtOperationScopeRule] = &[
    crdt_rule("workspace.setCustomCss", &["workspace.write.crdt"]),
    crdt_rule(
        "crdt.flush",
        &["documents.write.crdt", "workspace.write.crdt"],
    ),
    crdt_rule("workspace.createDocument", &["workspace.write.crdt"]),
    crdt_rule("workspace.updateDocument", &["workspace.write.crdt"]),
    crdt_rule("workspace.createFolder", &["workspace.write.crdt"]),
    crdt_rule("workspace.updateFolder", &["workspace.write.crdt"]),
    crdt_rule("workspace.moveFolder", &["workspace.write.crdt"]),
    crdt_rule("workspace.moveDocuments", &["workspace.write.crdt"]),
    crdt_rule(
        "workspace.deleteDocument",
        &["documents.delete.crdt", "workspace.delete.crdt"],
    ),
    crdt_rule("workspace.deleteFolder", &["workspace.delete.crdt"]),
    crdt_rule(
        "workspace.putArtifact",
        &["workspace.write.crdt", "artifacts.write"],
    ),
    crdt_rule(
        "workspace.deleteArtifact",
        &["workspace.delete.crdt", "artifacts.delete"],
    ),
    crdt_rule("workspace.createWire", &["wires.write"]),
    crdt_rule("workspace.refreshWire", &["wires.write"]),
    crdt_rule("workspace.deleteWire", &["wires.delete"]),
    crdt_rule("document.write", &["documents.write.crdt"]),
    crdt_rule("document.createOnce", &["documents.write.crdt", "workspace.write.crdt"]),
    crdt_rule("artifact.mutateText", &["artifacts.write", "workspace.write.crdt"]),
    crdt_rule("document.editComment", &["documents.write.crdt"]),
    crdt_rule("document.liveProjection", &["documents.write.crdt"]),
    // Unit G4: seeding the Mithras Flow board is a document write (the brief's
    // "scope rule like other document.* writes").
    crdt_rule("flow.seed", &["documents.write.crdt"]),
    crdt_rule("block.insert", &["documents.write.crdt"]),
    crdt_rule("block.update", &["documents.write.crdt"]),
    crdt_rule("block.editText", &["documents.write.crdt"]),
    crdt_rule("block.delete", &["documents.delete.crdt"]),
    crdt_rule(
        "document.batchPrepare",
        &["documents.write.crdt", "workspace.write.crdt"],
    ),
    crdt_rule(
        "document.batchRegister",
        &["documents.write.crdt", "workspace.write.crdt"],
    ),
    crdt_rule(
        "document.ingestMarkdownOriginal",
        &[
            "artifacts.ingest",
            "documents.write.crdt",
            "workspace.write.crdt",
        ],
    ),
    crdt_rule(
        "document.uploadIngest",
        &[
            "artifacts.ingest",
            "documents.write.crdt",
            "workspace.write.crdt",
        ],
    ),
    crdt_rule("graph.importArchive", &["graphs.import"]),
    crdt_rule("import.vault", &["graphs.import"]),
    crdt_rule(
        "import.webClip",
        &[
            "imports.web.write",
            "documents.write.crdt",
            "workspace.write.crdt",
        ],
    ),
];

const fn crdt_rule(kind: &'static str, scopes: &'static [&'static str]) -> CrdtOperationScopeRule {
    CrdtOperationScopeRule { kind, scopes }
}

pub(crate) fn crdt_operation_scope_rules() -> &'static [CrdtOperationScopeRule] {
    CRDT_OPERATION_SCOPE_RULES
}

pub(crate) fn crdt_operation_scopes(kind: &str) -> Option<Vec<&'static str>> {
    CRDT_OPERATION_SCOPE_RULES
        .iter()
        .find(|rule| rule.kind == kind)
        .map(|rule| rule.scopes.to_vec())
}

pub(crate) fn mcp_tool_required_scope_mode(name: &str) -> &'static str {
    local_mcp_tool_scope_entry(name)
        .and_then(|entry| entry.required_scope_mode.as_deref())
        .unwrap_or(STATIC_SCOPE_SELECTION_MODE)
}

pub(crate) fn mcp_tool_scopes(name: &str) -> Vec<&'static str> {
    local_mcp_tool_scope_entry(name)
        .map(|entry| entry.scopes.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

fn local_mcp_tool_scope_entry(name: &str) -> Option<&'static LocalMcpToolScopeEntry> {
    local_mcp_tool_scope_entries()
        .iter()
        .find(|entry| entry.name == name)
}

fn local_mcp_tool_scope_entries() -> &'static [LocalMcpToolScopeEntry] {
    LOCAL_MCP_TOOL_SCOPE_ENTRIES
        .get_or_init(|| {
            let surface: LocalLoopbackSurface = serde_json::from_str(LOCAL_LOOPBACK_SURFACE_JSON)
                .expect("embedded local loopback surface registry must be valid JSON");
            surface.mcp_tools
        })
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tool_scope_map_covers_mutating_representatives() {
        assert_eq!(mcp_tool_scopes("list_graphs"), vec!["graphs.read"]);
        assert!(mcp_tool_scopes("write_document").contains(&"documents.write.crdt"));
        assert!(mcp_tool_scopes("delete").contains(&"workspace.delete.crdt"));
        assert!(mcp_tool_scopes("rdf_load").contains(&"rdf.load"));
        assert!(mcp_tool_scopes("semantic_search").contains(&"search.semantic.read"));
        assert!(mcp_tool_scopes("memory_semantic_recall").contains(&"memory.read"));
        assert!(mcp_tool_scopes("memory_semantic_recall").contains(&"search.semantic.read"));
        assert!(mcp_tool_scopes("semantic_reason").contains(&"search.semantic.read"));
        assert!(mcp_tool_scopes("graph_intuition").contains(&"search.semantic.read"));
        assert!(mcp_tool_scopes("graph_intuition").contains(&"services.proxy"));
        assert!(mcp_tool_scopes("crdt_operation").contains(&"graphs.import"));
        assert_eq!(
            mcp_tool_required_scope_mode("crdt_operation"),
            "operation-kind"
        );
        assert_eq!(
            mcp_tool_required_scope_mode("write_document"),
            STATIC_SCOPE_SELECTION_MODE
        );
    }

    #[test]
    fn crdt_operation_scope_map_covers_known_operation_families() {
        assert!(crdt_operation_scope_rules()
            .iter()
            .any(|rule| rule.kind == "document.write"));
        assert_eq!(
            crdt_operation_scopes("document.write"),
            Some(vec!["documents.write.crdt"])
        );
        assert_eq!(
            crdt_operation_scopes("workspace.deleteDocument"),
            Some(vec!["documents.delete.crdt", "workspace.delete.crdt"])
        );
        assert_eq!(
            crdt_operation_scopes("workspace.putArtifact"),
            Some(vec!["workspace.write.crdt", "artifacts.write"])
        );
        assert_eq!(
            crdt_operation_scopes("import.webClip"),
            Some(vec![
                "imports.web.write",
                "documents.write.crdt",
                "workspace.write.crdt"
            ])
        );
        assert_eq!(crdt_operation_scopes("unknown.operation"), None);
    }

    #[test]
    fn crdt_operation_tool_scope_union_matches_operation_rules() {
        use std::collections::BTreeSet;
        let mut union = BTreeSet::new();
        for rule in crdt_operation_scope_rules() {
            for scope in rule.scopes {
                union.insert(*scope);
            }
        }
        let declared = mcp_tool_scopes("crdt_operation");
        let declared_set = declared.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(declared.len(), declared_set.len(), "declared generic scopes must not contain duplicates");
        // Authorization requires every scope by membership; role selection
        // takes their maximum. Neither promises first-encounter ordering.
        // The early workspace.setCustomCss rule changes only that ordering.
        assert_eq!(declared_set, union, "declared generic scopes must equal the complete operation union");
    }
}
