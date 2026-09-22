use crate::runtime_config::DOCUMENT_SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateDocumentInput {
    pub(super) graph_id: String,
    pub(super) title: String,
    pub(super) document_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SaveDocumentInput {
    pub(super) graph_id: String,
    pub(super) document_id: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) tiptap_xml: Option<String>,
    pub(super) tiptap_json: Option<serde_json::Value>,
    pub(super) ydoc_update_base64: Option<String>,
    pub(super) tree: Option<DocumentTreeSnapshot>,
    pub(super) blocks: Option<Vec<BlockSnapshot>>,
    /// Workspace-declared document kind (unit G3). `Some("flow-board")` marks
    /// a Mithras Flow board whose content lives in the Y.Doc's `resource`/
    /// `scene` roots, never in TipTap fields; absent = TipTap document.
    #[serde(default)]
    pub(super) document_kind: Option<String>,
    pub(super) trace_operation_id: Option<String>,
    /// Optimistic-concurrency revision guard (A2 item 9).
    /// If present: increment only if current == expected (normal write) or
    /// current == expected + 1 (idempotent replay).  Absent: increment
    /// unconditionally (backwards-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) expected_revision: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceRecord {
    pub(super) graph_id: String,
    pub(super) ydoc_update_base64: String,
    pub(super) ydoc_state_path: String,
    pub(super) snapshot: Option<serde_json::Value>,
    pub(super) snapshot_path: String,
    pub(super) updated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SaveWorkspaceInput {
    pub(super) graph_id: String,
    pub(super) ydoc_update_base64: String,
    pub(super) snapshot: Option<serde_json::Value>,
    pub(super) trace_operation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SaveWorkspaceYDocStateInput {
    pub(super) graph_id: String,
    pub(super) ydoc_update_base64: String,
    pub(super) trace_operation_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentRecord {
    pub(super) document_id: String,
    pub(super) graph_id: String,
    pub(super) title: String,
    #[serde(default)]
    pub(super) revision: u64,
    pub(super) body: String,
    pub(super) origin: String,
    pub(super) provider_id: String,
    pub(super) local_path: String,
    pub(super) rdf_subject: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) capabilities: Vec<String>,
    #[serde(default = "default_document_schema_version")]
    pub(super) schema_version: u32,
    #[serde(default)]
    pub(super) tiptap_xml: String,
    #[serde(default)]
    pub(super) tiptap_json: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) ydoc_update_base64: String,
    #[serde(default)]
    pub(super) ydoc_state_path: String,
    #[serde(default)]
    pub(super) tree: Option<DocumentTreeSnapshot>,
    #[serde(default)]
    pub(super) blocks: Vec<BlockSnapshot>,
    #[serde(default)]
    pub(super) rdf_triple_count: usize,
    /// The workspace-declared `documentKind` flag, persisted so cold paths
    /// (history snapshots, restore, RDF tails) can take the flow-board branch
    /// without consulting the workspace Y.Doc (unit G3). Skipped when `None`
    /// so every existing TipTap `document.json` round-trips byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) document_kind: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentTreeSnapshot {
    pub(super) root: TreeNodeSnapshot,
    pub(super) doc_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct TreeNodeSnapshot {
    pub(super) kind: String,
    pub(super) tag_name: Option<String>,
    pub(super) text_content: Option<String>,
    pub(super) attributes: TreeNodeAttributes,
    pub(super) children: Vec<TreeNodeSnapshot>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct TreeNodeAttributes {
    pub(super) block_id: Option<String>,
    pub(super) level: Option<i64>,
    pub(super) href: Option<String>,
    pub(super) target: Option<String>,
    pub(super) language: Option<String>,
    pub(super) checked: Option<bool>,
    pub(super) footnote_content: Option<String>,
    pub(super) annotation_id: Option<String>,
    pub(super) wire_id: Option<String>,
    pub(super) src: Option<String>,
    pub(super) alt: Option<String>,
    pub(super) extra: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct BlockSnapshot {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) block_type: String,
    pub(super) content: String,
    pub(super) parent_id: Option<String>,
    pub(super) order: f64,
    pub(super) level: Option<i64>,
    pub(super) checked: Option<bool>,
    pub(super) language: Option<String>,
    pub(super) marks: Vec<InlineMarkSnapshot>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct InlineMarkSnapshot {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) mark_type: String,
    pub(super) start: u32,
    pub(super) end: u32,
    pub(super) href: Option<String>,
    pub(super) target_doc_id: Option<String>,
    pub(super) label: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SaveDocumentYDocStateInput {
    pub(super) graph_id: String,
    pub(super) document_id: String,
    pub(super) ydoc_update_base64: String,
    pub(super) trace_operation_id: Option<String>,
}

fn default_document_schema_version() -> u32 {
    DOCUMENT_SCHEMA_VERSION
}
