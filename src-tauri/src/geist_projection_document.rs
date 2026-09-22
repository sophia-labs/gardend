use crate::{
    document_projection_service::render_block_content,
    document_service::{
        read_document_record, write_document_record, BlockSnapshot, DocumentRecord,
    },
    paths::{document_ydoc_state_path, ensure_document_dirs},
    rdf::document_subject,
    rdf_service::{
        document_tree_triples, materialize_document_record_with_triples, open_graph_store,
    },
    runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    storage::display_path,
};

pub(crate) fn write_geist_projection_document(
    graph_dir: &std::path::Path,
    graph_id: &str,
    document_id: &str,
    title: &str,
    blocks: Vec<BlockSnapshot>,
    capabilities: Vec<String>,
) -> Result<(), String> {
    let (document_dir, _) = ensure_document_dirs(graph_dir, document_id)?;
    let existing = document_dir
        .join("document.json")
        .is_file()
        .then(|| read_document_record(graph_dir, &document_dir.join("document.json")))
        .transpose()?;
    let now = crate::clock::timestamp();
    let body = blocks
        .iter()
        .filter(|block| !block.content.trim().is_empty())
        .map(|block| block.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let tiptap_xml = blocks
        .iter()
        .map(|block| render_block_content(block, "xml"))
        .collect::<Vec<_>>()
        .join("");
    let mut document = DocumentRecord {
        document_id: document_id.to_string(),
        graph_id: graph_id.to_string(),
        title: title.to_string(),
        revision: existing
            .as_ref()
            .map(|document| document.revision.saturating_add(1))
            .unwrap_or(1),
        body,
        origin: LOCAL_GRAPH_ORIGIN.to_string(),
        provider_id: LOCAL_PROVIDER_ID.to_string(),
        local_path: display_path(&document_dir),
        rdf_subject: document_subject(document_id),
        created_at: existing
            .as_ref()
            .map(|document| document.created_at.clone())
            .unwrap_or_else(|| now.clone()),
        updated_at: now,
        capabilities,
        schema_version: DOCUMENT_SCHEMA_VERSION,
        tiptap_xml,
        tiptap_json: None,
        ydoc_update_base64: String::new(),
        ydoc_state_path: display_path(&document_ydoc_state_path(graph_dir, document_id)),
        tree: None,
        blocks,
        rdf_triple_count: 0,
        document_kind: None,
    };
    let tree_triples = document_tree_triples(&document);
    document.rdf_triple_count = tree_triples.len();
    write_document_record(graph_dir, &document)?;
    let store = open_graph_store(graph_dir)?;
    // MO-flip DEFERRED: this caller sets `tree: None` and computes `tree_triples` out of
    // band, so `reconcile_document_record` would derive an EMPTY tree and not reproduce
    // this footprint. Stays on the old wholesale path until a `reconcile_*_with_triples`
    // variant (or `.tree` population) exists.
    materialize_document_record_with_triples(&store, &document, &tree_triples)
}
