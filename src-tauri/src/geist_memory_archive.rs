use crate::{
    document_projection_service::render_block_content,
    document_service::{write_document_record, BlockSnapshot, DocumentRecord},
    geist_memory_store::{memory_text, LocalMemoryRecord},
    paths::{document_ydoc_state_path, ensure_document_dirs},
    rdf::document_subject,
    rdf_service::{
        document_tree_triples, materialize_document_record_with_triples, open_graph_store,
    },
    runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    storage::display_path,
};
use std::path::Path;

pub(crate) fn write_memory_archive_document(
    graph_dir: &Path,
    graph_id: &str,
    archive_doc_id: &str,
    archived_at: &str,
    kept: usize,
    memories: &[LocalMemoryRecord],
) -> Result<(), String> {
    let title = format!("Memory Archive {archived_at}");
    let (document_dir, _) = ensure_document_dirs(graph_dir, archive_doc_id)?;

    let mut blocks = Vec::new();
    blocks.push(BlockSnapshot {
        id: "archive-heading".to_string(),
        block_type: "heading".to_string(),
        content: title.clone(),
        parent_id: None,
        order: 0.0,
        level: Some(1),
        checked: None,
        language: None,
        marks: Vec::new(),
    });
    blocks.push(BlockSnapshot {
        id: "archive-summary".to_string(),
        block_type: "paragraph".to_string(),
        content: format!(
            "Archived {} memories. Kept {} most recently active.",
            memories.len(),
            kept
        ),
        parent_id: None,
        order: 1.0,
        level: None,
        checked: None,
        language: None,
        marks: Vec::new(),
    });
    for (index, memory) in memories.iter().enumerate() {
        blocks.push(BlockSnapshot {
            id: format!("memory-archive-{}", memory.number),
            block_type: "paragraph".to_string(),
            content: memory_text(memory),
            parent_id: None,
            order: (index + 2) as f64,
            level: None,
            checked: None,
            language: None,
            marks: Vec::new(),
        });
    }

    let body = blocks
        .iter()
        .map(|block| block.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let tiptap_xml = blocks
        .iter()
        .map(|block| render_block_content(block, "xml"))
        .collect::<Vec<_>>()
        .join("");
    let now = archived_at.to_string();
    let mut document = DocumentRecord {
        document_id: archive_doc_id.to_string(),
        graph_id: graph_id.to_string(),
        title,
        revision: 1,
        body,
        origin: LOCAL_GRAPH_ORIGIN.to_string(),
        provider_id: LOCAL_PROVIDER_ID.to_string(),
        local_path: display_path(&document_dir),
        rdf_subject: document_subject(archive_doc_id),
        created_at: now.clone(),
        updated_at: now,
        capabilities: vec![
            "document.local.read".to_string(),
            "document.local.materialize.rdf".to_string(),
            "memory.local.archive".to_string(),
        ],
        schema_version: DOCUMENT_SCHEMA_VERSION,
        tiptap_xml,
        tiptap_json: None,
        ydoc_update_base64: String::new(),
        ydoc_state_path: display_path(&document_ydoc_state_path(graph_dir, archive_doc_id)),
        tree: None,
        blocks,
        rdf_triple_count: 0,
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
