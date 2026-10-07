//! The one on-disk storage-format version for a Garden profile / durable dir.
//!
//! Garden has no single profile-level schema number: each store carries its
//! own `*_SCHEMA_VERSION` constant (see `docs/storage-format.md`). Release
//! tooling needs one number to answer "can this binary open the bytes the
//! running one wrote?", so `STORAGE_FORMAT` is that number, and the test
//! below pins every per-store schema constant: changing any of them fails
//! the test until `STORAGE_FORMAT` is bumped and the pin updated together.
//!
//! `gardend --version --json` reports it as `storageFormat`, each release's
//! `release.json` carries it, and `gardend update` refuses a release whose
//! `storageFormat` differs from the running binary's.

/// Bump whenever any per-store on-disk schema changes (the pin test says so).
pub const STORAGE_FORMAT: u32 = 1;

#[cfg(test)]
mod tests {
    use super::STORAGE_FORMAT;
    use std::path::Path;

    /// Every `const *SCHEMA_VERSION` in the crate, as `file:NAME=value`,
    /// frozen at the `STORAGE_FORMAT` it belongs to.
    const PINNED_AT_FORMAT_1: &[&str] = &[
        "artifact_revisions.rs:SCHEMA_VERSION=1",
        "crdt_engine/block_ops.rs:LEDGER_SCHEMA_VERSION=1.0",
        "crdt_operation_journal.rs:CRDT_OPERATION_JOURNAL_SCHEMA_VERSION=1",
        "document_history_store.rs:DOCUMENT_TAIL_COMMIT_SCHEMA_VERSION=2",
        "document_history_store.rs:HISTORY_STORE_SCHEMA_VERSION=3",
        "document_tombstone_store.rs:DOCUMENT_TOMBSTONE_SCHEMA_VERSION=1",
        // Two new stores from the Files rudiments (2026-10-06), pinned at format 1: no existing
        // store changed, and a format-1 binary that predates them opens the profile and ignores them.
        "files_service.rs:TRASH_SCHEMA_VERSION=1",
        "geist_memory_store.rs:MEMORY_STORE_SCHEMA_VERSION=1",
        "geist_song_store.rs:SONG_STORE_SCHEMA_VERSION=1",
        "hosted_credentials.rs:SCHEMA_VERSION=1",
        "hosted_mode.rs:SCHEMA_VERSION=1",
        "local_provider_keys.rs:SCHEMA_VERSION=1",
        "operation_completion_ledger.rs:OPERATION_COMPLETION_LEDGER_SCHEMA_VERSION=1",
        "pdf_docling_runtime_manifest.rs:DOCLING_RUNTIME_SCHEMA_VERSION=1",
        "pdf_pipeline_config.rs:PDF_PIPELINE_SCHEMA_VERSION=1",
        "runtime_config.rs:DOCUMENT_SCHEMA_VERSION=1",
        "salience_value_store.rs:VALUE_STORE_SCHEMA_VERSION=1",
        "semantic_models.rs:SEMANTIC_INDEX_SCHEMA_VERSION=1",
        "source_sync.rs:FILE_VIEWS_RECORD_SCHEMA_VERSION=1",
        "source_sync.rs:SOURCE_SYNC_SCHEMA_VERSION=1",
        "time_travel_store.rs:RESTORE_POINT_INDEX_SCHEMA_VERSION=1",
        "time_travel_types.rs:RESTORE_POINT_MANIFEST_SCHEMA_VERSION=2",
        "time_travel_types.rs:RESTORE_POINT_MIN_RESTORABLE_SCHEMA_VERSION=2",
    ];

    fn scan(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                scan(&path, root, out);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            for line in text.lines() {
                let line = line.trim();
                let Some(idx) = line.find("const ") else {
                    continue;
                };
                if line.starts_with("//") {
                    continue;
                }
                let rest = &line[idx + "const ".len()..];
                let Some((name, tail)) = rest.split_once(':') else {
                    continue;
                };
                let name = name.trim();
                if !name.ends_with("SCHEMA_VERSION") {
                    continue;
                }
                let Some((_, value)) = tail.split_once('=') else {
                    continue;
                };
                let value = value.trim().trim_end_matches(';').trim();
                out.push(format!("{rel}:{name}={value}"));
            }
        }
    }

    #[test]
    fn storage_format_pins_every_per_store_schema_version() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        scan(&root, &root, &mut found);
        found.sort();
        let mut pinned: Vec<String> = PINNED_AT_FORMAT_1.iter().map(|s| s.to_string()).collect();
        pinned.sort();
        assert_eq!(
            STORAGE_FORMAT, 1,
            "update the pin's name with the new format"
        );
        assert_eq!(
            found, pinned,
            "a per-store on-disk schema version changed: bump STORAGE_FORMAT in \
             src/storage_format.rs and re-pin this list (releases with a different \
             storageFormat are refused by `gardend update`)"
        );
    }
}
