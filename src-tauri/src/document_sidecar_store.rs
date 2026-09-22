use crate::{
    document_types::DocumentRecord,
    paths::{document_ydoc_dir, document_ydoc_state_path},
    storage::{create_dir_all, read_bytes, write_bytes, write_json},
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::path::Path;
use yrs::{Doc, ReadTxn, StateVector, Transact};

/// A Y.Doc with no application content still has a valid encoded update.
///
/// Zero bytes are an internal legacy sentinel used by older Garden profiles,
/// but they are not a Yjs update and cannot cross the blob/source-sync
/// boundary: `Y.applyUpdate` correctly rejects them. Keep one canonical
/// representation for new state and for compatibility reads of that sentinel.
pub(super) fn empty_ydoc_update_v1() -> Vec<u8> {
    let doc = Doc::new();
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    update
}

pub(super) fn empty_ydoc_update_base64() -> String {
    BASE64_STANDARD.encode(empty_ydoc_update_v1())
}

pub(super) fn canonicalize_ydoc_update_bytes(bytes: Vec<u8>) -> Vec<u8> {
    if bytes.is_empty() {
        empty_ydoc_update_v1()
    } else {
        bytes
    }
}

pub(super) fn read_ydoc_update_base64(state_path: &Path) -> Result<Option<String>, String> {
    if !state_path.is_file() {
        return Ok(None);
    }
    let state_bytes = canonicalize_ydoc_update_bytes(read_bytes(state_path)?);
    Ok(Some(BASE64_STANDARD.encode(state_bytes)))
}

pub(super) fn write_document_state_files(
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<(), String> {
    let ydoc_dir = document_ydoc_dir(graph_dir, &document.document_id);
    create_dir_all(&ydoc_dir)?;
    write_ydoc_update(
        &document_ydoc_state_path(graph_dir, &document.document_id),
        &document.ydoc_update_base64,
    )?;

    if !document.tiptap_xml.is_empty() {
        write_bytes(&ydoc_dir.join("tiptap.xml"), document.tiptap_xml.as_bytes())
            .map_err(|error| format!("write TipTap XML: {error}"))?;
    }
    if let Some(tiptap_json) = &document.tiptap_json {
        write_json(&ydoc_dir.join("tiptap.json"), tiptap_json)?;
    }
    if let Some(tree) = &document.tree {
        write_json(&ydoc_dir.join("tree.json"), tree)?;
    }
    write_json(&ydoc_dir.join("blocks.json"), &document.blocks)?;

    Ok(())
}

pub(super) fn write_ydoc_update(state_path: &Path, ydoc_update_base64: &str) -> Result<(), String> {
    crate::document_body_availability::require_state_path_available(state_path)?;
    if let Some(parent) = state_path.parent() {
        create_dir_all(parent)?;
    }
    let state_bytes = if ydoc_update_base64.trim().is_empty() {
        empty_ydoc_update_v1()
    } else {
        BASE64_STANDARD
            .decode(ydoc_update_base64)
            .map_err(|error| format!("decode Yjs update: {error}"))?
    };
    write_bytes(state_path, &state_bytes).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;
    use yrs::{updates::decoder::Decode, Update};

    #[test]
    fn canonical_empty_update_is_nonzero_and_decodable() {
        let bytes = empty_ydoc_update_v1();

        assert!(!bytes.is_empty());
        Update::decode_v1(&bytes).expect("canonical empty state is a valid Yjs update");
    }

    #[test]
    fn empty_input_persists_a_valid_empty_update() {
        let dir = std::env::temp_dir().join(format!("sophia-empty-ydoc-{}", Uuid::new_v4()));
        let state_path = dir.join("update-v1.bin");

        write_ydoc_update(&state_path, "").expect("write empty Y.Doc state");
        let bytes = fs::read(&state_path).expect("read empty Y.Doc state");

        assert!(!bytes.is_empty());
        Update::decode_v1(&bytes).expect("persisted empty state is a valid Yjs update");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_zero_byte_sidecar_reads_as_a_valid_empty_update() {
        let dir = std::env::temp_dir().join(format!("sophia-legacy-empty-ydoc-{}", Uuid::new_v4()));
        let state_path = dir.join("update-v1.bin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&state_path, []).unwrap();

        let encoded = read_ydoc_update_base64(&state_path)
            .expect("read legacy empty state")
            .expect("sidecar exists");
        let bytes = BASE64_STANDARD.decode(encoded).unwrap();

        assert!(!bytes.is_empty());
        Update::decode_v1(&bytes).expect("compatibility state is a valid Yjs update");
        fs::remove_dir_all(dir).unwrap();
    }
}
