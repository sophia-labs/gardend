pub(super) use crate::document_sidecar_store::write_ydoc_update;
pub(super) use crate::workspace_record_store::read_workspace_record;
use crate::{
    document_sidecar_store::{read_ydoc_update_base64, write_document_state_files},
    document_types::DocumentRecord,
    ids::validate_local_id,
    paths::{document_ydoc_state_path, documents_dir, ensure_document_dirs},
    runtime_config::DOCUMENT_SCHEMA_VERSION,
    storage::{create_dir_all, display_path, read_json, write_json},
};
use std::{fs, path::Path};

#[cfg(test)]
thread_local! {
    // Actual reader-dispatch counts on this test thread, not filesystem byte
    // or allocator measurements. There is no production counter/global state.
    static RECORD_READ_COUNTS: std::cell::Cell<(usize, usize)> = const {
        std::cell::Cell::new((0, 0))
    };
}

pub(super) fn read_document_record(
    graph_dir: &Path,
    manifest_path: &Path,
) -> Result<DocumentRecord, String> {
    let document = read_json::<DocumentRecord>(manifest_path)?;
    validate_document_record_ids(&document)?;
    crate::document_tombstone_store::require_document_not_tombstoned(
        graph_dir,
        &document.document_id,
    )?;
    hydrate_document_record(graph_dir, document, SidecarBackfill::Allowed)
}

/// The workspace title sweep needs history only when it will actually save a
/// rename. Validate the same complete manifest and tombstone as the hydrating
/// reader, then decide before any sidecar read/backfill. A changed title uses
/// that SAME parsed record, rather than a second manifest read with a new
/// revision/content observation. The existing save path owns the rename.
pub(super) fn read_document_record_if_title_changed(
    graph_dir: &Path,
    manifest_path: &Path,
    title: &str,
) -> Result<Option<DocumentRecord>, String> {
    let document = read_json::<DocumentRecord>(manifest_path)?;
    validate_document_record_ids(&document)?;
    crate::document_tombstone_store::require_document_not_tombstoned(
        graph_dir,
        &document.document_id,
    )?;
    if document.title == title {
        return Ok(None);
    }
    hydrate_document_record(graph_dir, document, SidecarBackfill::Allowed).map(Some)
}

/// Transient compact-inventory projection, NOT another persisted authority.
/// No body, TipTap tree, block payload or Y.Doc history escapes the walk in
/// this type. Manifests are still fully parsed/validated one at a time: this
/// bounds retained content to one record, not total source bytes or CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DocumentInventoryRecord {
    pub(super) document_id: String,
    pub(super) title: String,
    pub(super) updated_at: String,
    pub(super) block_count: usize,
    pub(super) rdf_triple_count: usize,
}

impl From<DocumentRecord> for DocumentInventoryRecord {
    fn from(document: DocumentRecord) -> Self {
        Self {
            document_id: document.document_id,
            title: document.title,
            updated_at: document.updated_at,
            block_count: document.blocks.len(),
            rdf_triple_count: document.rdf_triple_count,
        }
    }
}

/// COLD read of one document record: everything `read_document_record` yields
/// EXCEPT the Y.Doc update payload. Same manifest parse, same embedded-ID
/// validation, same tombstone fence, same schema stamp and derived
/// `ydoc_state_path` — but NO sidecar I/O, NO legacy inline-update backfill
/// write, and the inline `ydoc_update_base64` the manifest may carry is
/// dropped immediately after parse.
///
/// WHY THIS EXISTS (the gardend OOM discipline): on the canary graph a
/// document's Y.Doc update history can be tens of MB (repeated full
/// rewrites), and `read_document_record` hydrates the FULL history into a
/// base64 `String` for every caller. Every all-documents walk built on it
/// (the old hydrating `read_graph_documents`: workspace projection, MCP
/// search, salience, orientation, semantic refresh, the RDF seed walk,
/// time-travel capture) therefore paid O(total Y.Doc history) memory for
/// reads that only ever consume the projected content fields
/// (title/body/blocks/tree/tiptap).
/// Callers that genuinely need the update payload (editor blob routes,
/// single-document `read_document`, persistence) keep the hydrating read.
pub(super) fn read_document_record_cold(
    graph_dir: &Path,
    manifest_path: &Path,
) -> Result<DocumentRecord, String> {
    #[cfg(test)]
    RECORD_READ_COUNTS.with(|counts| {
        let (cold, hydration) = counts.get();
        counts.set((cold + 1, hydration));
    });
    let mut document = read_json::<DocumentRecord>(manifest_path)?;
    validate_document_record_ids(&document)?;
    crate::document_tombstone_store::require_document_not_tombstoned(
        graph_dir,
        &document.document_id,
    )?;
    document.schema_version = DOCUMENT_SCHEMA_VERSION;
    document.ydoc_state_path =
        display_path(&document_ydoc_state_path(graph_dir, &document.document_id));
    // Drop the inline history payload NOW: the transient parse cost is
    // unavoidable while manifests carry it, but no cold caller may retain it.
    document.ydoc_update_base64 = String::new();
    Ok(document)
}

/// Read, validate, and hydrate a cold record for one expected route identity
/// from a single manifest parse. A valid but mismatched record returns `None`;
/// invalid embedded IDs are errors. Crucially, neither case may derive or
/// touch a sidecar path from the rejected manifest.
pub(super) fn read_document_record_for_identity(
    graph_dir: &Path,
    manifest_path: &Path,
    expected_graph_id: &str,
    expected_document_id: &str,
    backfill: SidecarBackfill,
) -> Result<Option<DocumentRecord>, String> {
    validate_local_id(expected_graph_id, "graph_id")?;
    validate_local_id(expected_document_id, "document_id")?;
    crate::document_tombstone_store::require_document_not_tombstoned(
        graph_dir,
        expected_document_id,
    )?;
    let document = read_json::<DocumentRecord>(manifest_path)?;
    validate_document_record_ids(&document)?;
    if document.graph_id != expected_graph_id || document.document_id != expected_document_id {
        return Ok(None);
    }
    hydrate_document_record(graph_dir, document, backfill).map(Some)
}

fn validate_document_record_ids(document: &DocumentRecord) -> Result<(), String> {
    validate_local_id(&document.graph_id, "graph_id")?;
    validate_local_id(&document.document_id, "document_id")
}

/// Whether a hydrating read may backfill a missing sidecar from the
/// manifest's legacy inline payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SidecarBackfill {
    /// Write the legacy inline payload into the missing sidecar. Only for
    /// callers whose lease may write (the hot read/save paths).
    Allowed,
    /// Never write. The room-open path runs under a `SharedLease`, whose
    /// type-level claim is "cannot write" — the read serves the manifest's
    /// inline bytes as-is, and a legacy document backfills on its first
    /// hot-write touch instead.
    ReadOnly,
}

fn hydrate_document_record(
    graph_dir: &Path,
    mut document: DocumentRecord,
    backfill: SidecarBackfill,
) -> Result<DocumentRecord, String> {
    #[cfg(test)]
    RECORD_READ_COUNTS.with(|counts| {
        let (cold, hydration) = counts.get();
        counts.set((cold, hydration + 1));
    });
    document.schema_version = DOCUMENT_SCHEMA_VERSION;
    document.ydoc_state_path =
        display_path(&document_ydoc_state_path(graph_dir, &document.document_id));

    let state_path = document_ydoc_state_path(graph_dir, &document.document_id);
    if let Some(ydoc_update_base64) = read_ydoc_update_base64(&state_path)? {
        document.ydoc_update_base64 = ydoc_update_base64;
    } else if !document.ydoc_update_base64.is_empty() && backfill == SidecarBackfill::Allowed {
        let _durability_guard = crate::cell_durability::write_guard();
        write_ydoc_update(&state_path, &document.ydoc_update_base64)?;
    }

    Ok(document)
}

/// Full-content all-documents listing over [`read_document_record_cold`]: every record's
/// content projection with NO Y.Doc history hydration. This is the ONLY
/// full-content listing — the hydrating equivalent was removed on purpose:
/// a walk built on hydrating reads holds every document's full update
/// history (base64, ~1.33x raw) in one `Vec` simultaneously, which is
/// exactly the O(all histories) residency that OOM'd 8Gi gardend cells on
/// fat graphs. Callers that need one document's Y.Doc payload read THAT ONE
/// document via `read_document_record`/`read_document`.
pub(super) fn read_graph_documents_cold(graph_dir: &Path) -> Result<Vec<DocumentRecord>, String> {
    let mut documents = Vec::new();
    visit_graph_documents_cold(graph_dir, |document| documents.push(document))?;
    documents.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(documents)
}

pub(super) fn read_graph_document_inventory(
    graph_dir: &Path,
) -> Result<Vec<DocumentInventoryRecord>, String> {
    let mut documents = Vec::new();
    visit_graph_documents_cold(graph_dir, |document| {
        documents.push(DocumentInventoryRecord::from(document));
    })?;
    // Keep exactly the former stable, descending updated_at ordering. Count
    // joins relied on the first occurrence for duplicate embedded IDs.
    documents.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(documents)
}

fn visit_graph_documents_cold(
    graph_dir: &Path,
    mut visit: impl FnMut(DocumentRecord),
) -> Result<(), String> {
    let documents_dir = documents_dir(graph_dir);
    create_dir_all(&documents_dir)?;

    let entries =
        fs::read_dir(&documents_dir).map_err(|error| format!("read documents dir: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read document entry: {error}"))?;
        let manifest_path = entry.path().join("document.json");
        if !manifest_path.is_file() {
            continue;
        }
        let Some(document_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if crate::document_tombstone_store::document_is_tombstoned(graph_dir, &document_id)? {
            continue;
        }
        visit(read_document_record_cold(graph_dir, &manifest_path)?);
    }
    Ok(())
}

pub(super) fn write_document_record(
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<(), String> {
    crate::document_body_availability::require_available(graph_dir, &document.document_id)?;
    let _durability_guard = crate::cell_durability::write_guard();
    let (document_dir, _) = ensure_document_dirs(graph_dir, &document.document_id)?;
    write_document_state_files(graph_dir, document)?;
    write_json(&document_dir.join("document.json"), document).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document_types::{
            BlockSnapshot, DocumentTreeSnapshot, TreeNodeAttributes, TreeNodeSnapshot,
        },
        paths::{document_ydoc_dir, document_ydoc_state_path},
        rdf::document_subject,
        runtime_config::{LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    };
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_graph_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()))
    }

    fn test_document(graph_dir: &Path, document_id: &str) -> DocumentRecord {
        DocumentRecord {
            document_id: document_id.to_string(),
            graph_id: "graph-a".to_string(),
            title: "Document A".to_string(),
            revision: 1,
            body: "Document body".to_string(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: display_path(&documents_dir(graph_dir).join(document_id)),
            rdf_subject: document_subject(document_id),
            created_at: "1000".to_string(),
            updated_at: "2000".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: display_path(&document_ydoc_state_path(graph_dir, document_id)),
            tree: None,
            blocks: Vec::new(),
            rdf_triple_count: 0,
        }
    }

    fn write_manifest_only(graph_dir: &Path, document: &DocumentRecord) -> Result<PathBuf, String> {
        let document_dir = documents_dir(graph_dir).join(&document.document_id);
        fs::create_dir_all(&document_dir)
            .map_err(|error| format!("create test document dir: {error}"))?;
        let manifest_path = document_dir.join("document.json");
        write_json(&manifest_path, document)?;
        Ok(manifest_path)
    }

    #[test]
    fn workspace_inventory_unchanged_title_never_reads_or_backfills_history() {
        let graph_dir = temp_graph_dir("sophia-workspace-inventory-title");
        let mut document = test_document(&graph_dir, "doc-a");
        document.ydoc_update_base64 = BASE64_STANDARD.encode(b"legacy-history");
        let manifest = write_manifest_only(&graph_dir, &document).unwrap();
        let before = fs::read(&manifest).unwrap();
        let sidecar = document_ydoc_state_path(&graph_dir, "doc-a");

        RECORD_READ_COUNTS.with(|counts| counts.set((0, 0)));
        assert!(read_document_record_if_title_changed(&graph_dir, &manifest, &document.title)
            .unwrap().is_none());
        assert_eq!(RECORD_READ_COUNTS.with(std::cell::Cell::get), (0, 0));
        assert!(!sidecar.exists(), "equal title must not backfill legacy history");

        // A directory at the exact byte-file path makes legacy backfill fail.
        // The unchanged-title path must not enter hydration/backfill at all.
        fs::create_dir_all(&sidecar).unwrap();
        assert!(read_document_record(&graph_dir, &manifest).is_err());
        RECORD_READ_COUNTS.with(|counts| counts.set((0, 0)));
        assert!(read_document_record_if_title_changed(&graph_dir, &manifest, &document.title)
            .unwrap().is_none());
        assert_eq!(RECORD_READ_COUNTS.with(std::cell::Cell::get), (0, 0));
        assert!(sidecar.is_dir());
        assert_eq!(fs::read(&manifest).unwrap(), before);
        fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn workspace_inventory_changed_title_keeps_hydration_and_legacy_backfill() {
        let graph_dir = temp_graph_dir("sophia-workspace-inventory-rename");
        let mut document = test_document(&graph_dir, "doc-a");
        document.ydoc_update_base64 = BASE64_STANDARD.encode(b"legacy-history");
        let manifest = write_manifest_only(&graph_dir, &document).unwrap();
        let sidecar = document_ydoc_state_path(&graph_dir, "doc-a");

        RECORD_READ_COUNTS.with(|counts| counts.set((0, 0)));
        let record = read_document_record_if_title_changed(&graph_dir, &manifest, "Renamed")
            .unwrap().expect("rename retains the actual record for the save path");
        assert_eq!(record.title, document.title, "the reader must not save a rename");
        assert_eq!(record.body, document.body);
        assert_eq!(record.revision, document.revision);
        assert_eq!(fs::read(&sidecar).unwrap(), b"legacy-history");
        fs::write(&sidecar, b"current-sidecar").unwrap();
        let record = read_document_record_if_title_changed(&graph_dir, &manifest, "Renamed")
            .unwrap().unwrap();
        assert_eq!(record.ydoc_update_base64, BASE64_STANDARD.encode(b"current-sidecar"));
        assert_eq!(record.updated_at, document.updated_at);
        assert_eq!(RECORD_READ_COUNTS.with(std::cell::Cell::get), (0, 2));
        fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn workspace_inventory_equal_title_still_validates_the_record() {
        let graph_dir = temp_graph_dir("sophia-workspace-inventory-invalid");
        let document = test_document(&graph_dir, "doc-a");
        let manifest = write_manifest_only(&graph_dir, &document).unwrap();
        let mut value = serde_json::to_value(&document).unwrap();
        value["documentId"] = serde_json::json!("../escape");
        write_json(&manifest, &value).unwrap();
        assert!(read_document_record_if_title_changed(&graph_dir, &manifest, &document.title)
            .unwrap_err().contains("document_id"));
        value["documentId"] = serde_json::json!("doc-a");
        value["body"] = serde_json::json!({"malformed": true});
        write_json(&manifest, &value).unwrap();
        assert!(read_document_record_if_title_changed(&graph_dir, &manifest, &document.title)
            .is_err(), "metadata optimization must not silently bless malformed content");
        write_json(&manifest, &document).unwrap();
        crate::document_tombstone_store::write_document_tombstone_for_operation(
            &graph_dir, "doc-a", Some("inventory-delete"),
        ).unwrap();
        assert!(read_document_record_if_title_changed(&graph_dir, &manifest, &document.title)
            .unwrap_err().contains("tombstoned"));
        assert!(read_graph_document_inventory(&graph_dir).unwrap().is_empty());
        fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn workspace_inventory_retains_metadata_not_all_content_or_histories() {
        let graph_dir = temp_graph_dir("sophia-workspace-inventory-retention");
        for index in 0..4 {
            let mut document = test_document(&graph_dir, &format!("doc-{index}"));
            document.title = format!("Title {index}");
            document.updated_at = format!("{index}");
            document.body = "large body ".repeat(64 * 1024);
            document.ydoc_update_base64 = BASE64_STANDARD.encode(vec![42; 128 * 1024]);
            document.rdf_triple_count = index + 3;
            write_manifest_only(&graph_dir, &document).unwrap();
            // Poison every actual sidecar path: inventory must be cold.
            fs::create_dir_all(document_ydoc_state_path(&graph_dir, &document.document_id)).unwrap();
        }
        RECORD_READ_COUNTS.with(|counts| counts.set((0, 0)));
        let inventory = read_graph_document_inventory(&graph_dir).unwrap();
        assert_eq!(RECORD_READ_COUNTS.with(std::cell::Cell::get), (4, 0),
            "one actual cold reader dispatch per manifest, no hydration dispatch");
        let expected = read_graph_documents_cold(&graph_dir).unwrap()
            .into_iter().map(DocumentInventoryRecord::from).collect::<Vec<_>>();
        assert_eq!(inventory, expected, "same sorted metadata as the previous full-record walk");
        assert_eq!(inventory.len(), 4);
        assert_eq!(inventory[0].document_id, "doc-3");
        assert_eq!(inventory[0].rdf_triple_count, 6);
        let retained_strings = inventory.iter().map(|record|
            record.document_id.len() + record.title.len() + record.updated_at.len()
        ).sum::<usize>();
        assert!(retained_strings < 256, "only identity/title/time strings escape the walk");

        // The slice still scans and validates EVERY manifest, even a small
        // page's invisible document. It claims lower peak retention, not a
        // total-byte or parse budget.
        fs::write(documents_dir(&graph_dir).join("doc-0/document.json"), b"{ malformed").unwrap();
        assert!(read_graph_document_inventory(&graph_dir).is_err());
        fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn workspace_inventory_preserves_missing_manifest_and_duplicate_ordering() {
        let graph_dir = temp_graph_dir("sophia-workspace-inventory-listing");
        let mut old = test_document(&graph_dir, "doc-a");
        old.updated_at = "10".into();
        old.rdf_triple_count = 1;
        write_manifest_only(&graph_dir, &old).unwrap();
        let mut newer = old.clone();
        newer.updated_at = "20".into();
        newer.rdf_triple_count = 2;
        // Existing listing semantics accept valid embedded IDs even if the
        // directory name differs. Do not silently "fix" that in this slice.
        let second = documents_dir(&graph_dir).join("second-slot");
        fs::create_dir_all(&second).unwrap();
        write_json(&second.join("document.json"), &newer).unwrap();
        fs::create_dir_all(documents_dir(&graph_dir).join("missing-manifest")).unwrap();
        let inventory = read_graph_document_inventory(&graph_dir).unwrap();
        assert_eq!(inventory.len(), 2);
        assert_eq!(inventory[0].document_id, "doc-a");
        assert_eq!(inventory[0].rdf_triple_count, 2);
        assert_eq!(inventory[1].rdf_triple_count, 1);
        fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn read_document_record_prefers_sidecar_ydoc_state() {
        let graph_dir = temp_graph_dir("sophia-document-sidecar");
        let document_id = "doc-a";
        let mut document = test_document(&graph_dir, document_id);
        document.ydoc_update_base64 = BASE64_STANDARD.encode(b"manifest-state");
        let manifest_path = write_manifest_only(&graph_dir, &document).unwrap();
        let state_path = document_ydoc_state_path(&graph_dir, document_id);
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(&state_path, b"sidecar-state").unwrap();

        let record = read_document_record(&graph_dir, &manifest_path).unwrap();

        assert_eq!(
            record.ydoc_update_base64,
            BASE64_STANDARD.encode(b"sidecar-state")
        );
        assert_eq!(record.ydoc_state_path, display_path(&state_path));
        fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn read_document_record_backfills_legacy_inline_ydoc_update() {
        let graph_dir = temp_graph_dir("sophia-document-backfill");
        let document_id = "doc-a";
        let mut document = test_document(&graph_dir, document_id);
        document.ydoc_update_base64 = BASE64_STANDARD.encode(b"legacy-state");
        let manifest_path = write_manifest_only(&graph_dir, &document).unwrap();
        let state_path = document_ydoc_state_path(&graph_dir, document_id);
        assert!(!state_path.exists());

        let record = read_document_record(&graph_dir, &manifest_path).unwrap();

        assert_eq!(
            record.ydoc_update_base64,
            BASE64_STANDARD.encode(b"legacy-state")
        );
        assert_eq!(fs::read(&state_path).unwrap(), b"legacy-state");
        fs::remove_dir_all(graph_dir).unwrap();
    }

    /// The cold read is the no-hydration organism every all-documents walk
    /// must ride: (1) a legacy manifest carrying its full inline Y.Doc
    /// history comes back with content fields intact and the history payload
    /// DROPPED, and — unlike the hydrating read, whose legacy backfill writes
    /// the sidecar as a side effect — the sidecar file must NOT appear; (2) a
    /// present sidecar is left untouched and never loaded into the record.
    #[test]
    fn read_document_record_cold_drops_history_and_never_touches_the_sidecar() {
        let graph_dir = temp_graph_dir("sophia-document-cold-read");
        let document_id = "doc-a";
        let mut document = test_document(&graph_dir, document_id);
        document.body = "Cold body".to_string();
        document.ydoc_update_base64 = BASE64_STANDARD.encode(b"legacy-history-payload");
        let manifest_path = write_manifest_only(&graph_dir, &document).unwrap();
        let state_path = document_ydoc_state_path(&graph_dir, document_id);
        assert!(!state_path.exists());

        // (1) Legacy inline-only manifest: content intact, history dropped,
        // and NO backfill side effect (the hydrating read would have written
        // the sidecar here — see read_document_record_backfills_legacy_inline_ydoc_update).
        let record = read_document_record_cold(&graph_dir, &manifest_path).unwrap();
        assert_eq!(record.title, "Document A");
        assert_eq!(record.body, "Cold body");
        assert_eq!(record.ydoc_update_base64, "");
        assert_eq!(record.ydoc_state_path, display_path(&state_path));
        assert!(
            !state_path.exists(),
            "a cold read must never write the sidecar as a side effect"
        );

        // (2) Sidecar present: still not read, still not altered.
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(&state_path, b"sidecar-history").unwrap();
        let record = read_document_record_cold(&graph_dir, &manifest_path).unwrap();
        assert_eq!(record.ydoc_update_base64, "");
        assert_eq!(fs::read(&state_path).unwrap(), b"sidecar-history");
        fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn read_document_record_rejects_embedded_ids_before_sidecar_derivation() {
        let graph_dir = temp_graph_dir("sophia-document-embedded-id-safety");
        let safe_manifest = documents_dir(&graph_dir)
            .join("safe-route")
            .join("document.json");
        fs::create_dir_all(safe_manifest.parent().unwrap()).unwrap();
        let mut document = test_document(&graph_dir, "safe-route");
        document.document_id = "../../escape".to_string();
        document.ydoc_update_base64 = BASE64_STANDARD.encode(b"must-not-write");
        write_json(&safe_manifest, &document).unwrap();

        let error = read_document_record(&graph_dir, &safe_manifest)
            .expect_err("invalid embedded document id must be rejected");
        assert!(error.contains("document_id"), "{error}");
        assert!(!graph_dir.join("escape/update-v1.bin").exists());

        document.document_id = "safe-route".to_string();
        document.graph_id = "../escape-graph".to_string();
        write_json(&safe_manifest, &document).unwrap();
        let error = read_document_record(&graph_dir, &safe_manifest)
            .expect_err("invalid embedded graph id must be rejected");
        assert!(error.contains("graph_id"), "{error}");
        assert!(!document_ydoc_state_path(&graph_dir, "safe-route").exists());
        fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn write_document_record_writes_projection_sidecars() {
        let graph_dir = temp_graph_dir("sophia-document-write");
        let document_id = "doc-a";
        let mut document = test_document(&graph_dir, document_id);
        document.tiptap_xml = "<doc><p>Hello</p></doc>".to_string();
        document.tiptap_json = Some(serde_json::json!({
            "type": "doc",
            "content": [{ "type": "paragraph" }]
        }));
        document.ydoc_update_base64 = BASE64_STANDARD.encode(b"ydoc-state");
        document.tree = Some(DocumentTreeSnapshot {
            doc_id: document_id.to_string(),
            root: TreeNodeSnapshot {
                kind: "element".to_string(),
                tag_name: Some("doc".to_string()),
                text_content: None,
                attributes: TreeNodeAttributes::default(),
                children: vec![TreeNodeSnapshot {
                    kind: "text".to_string(),
                    tag_name: None,
                    text_content: Some("Hello".to_string()),
                    attributes: TreeNodeAttributes::default(),
                    children: Vec::new(),
                }],
            },
        });
        document.blocks = vec![BlockSnapshot {
            id: "block-a".to_string(),
            block_type: "paragraph".to_string(),
            content: "Hello".to_string(),
            parent_id: None,
            order: 0.0,
            level: None,
            checked: None,
            language: None,
            marks: Vec::new(),
        }];

        write_document_record(&graph_dir, &document).unwrap();

        let document_dir = documents_dir(&graph_dir).join(document_id);
        let ydoc_dir = document_ydoc_dir(&graph_dir, document_id);
        assert!(document_dir.join("document.json").is_file());
        assert_eq!(
            fs::read(document_ydoc_state_path(&graph_dir, document_id)).unwrap(),
            b"ydoc-state"
        );
        assert_eq!(
            fs::read_to_string(ydoc_dir.join("tiptap.xml")).unwrap(),
            "<doc><p>Hello</p></doc>"
        );
        let tiptap_json = read_json::<serde_json::Value>(&ydoc_dir.join("tiptap.json")).unwrap();
        assert_eq!(tiptap_json["type"], "doc");
        let tree = read_json::<DocumentTreeSnapshot>(&ydoc_dir.join("tree.json")).unwrap();
        assert_eq!(tree.doc_id, document_id);
        let blocks = read_json::<Vec<BlockSnapshot>>(&ydoc_dir.join("blocks.json")).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].id, "block-a");
        let manifest = read_json::<DocumentRecord>(&document_dir.join("document.json")).unwrap();
        assert_eq!(manifest.document_id, document_id);
        fs::remove_dir_all(graph_dir).unwrap();
    }
}
