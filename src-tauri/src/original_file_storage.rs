#[cfg(test)]
use crate::storage::{read_json, write_json};
use crate::{
    ids::safe_filename,
    original_file_manifest_store::{
        original_manifest_file_path, read_original_manifest, write_original_manifest,
    },
    original_file_types::OriginalFileManifest,
    storage::{copy_file_atomic, create_dir_all, display_path, read_bytes, write_bytes},
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::path::Path;
#[cfg(test)]
use std::{fs, path::PathBuf};
#[cfg(test)]
use uuid::Uuid;

pub(super) use crate::original_file_manifest_store::{
    rewrite_original_manifest_local_path, rewrite_original_manifests_under,
};

pub(super) fn save_original_file_to_dir(
    original_dir: &Path,
    filename: &str,
    mime_type: &str,
    data_base64: &str,
) -> Result<(OriginalFileManifest, Vec<u8>), String> {
    let bytes = BASE64_STANDARD
        .decode(data_base64.trim())
        .map_err(|error| format!("decode original file: {error}"))?;
    let manifest = save_original_bytes_to_dir(original_dir, filename, mime_type, &bytes)?;
    Ok((manifest, bytes))
}

pub(super) fn save_original_bytes_to_dir(
    original_dir: &Path,
    filename: &str,
    mime_type: &str,
    bytes: &[u8],
) -> Result<OriginalFileManifest, String> {
    let filename = safe_filename(filename);
    create_dir_all(original_dir)?;
    let file_path = original_dir.join(&filename);
    write_bytes(&file_path, bytes)
        .map_err(|error| format!("write original file {}: {error}", display_path(&file_path)))?;
    write_original_manifest(original_dir, &file_path, filename, mime_type, bytes.len())
}

pub(super) fn save_original_file_from_path_to_dir(
    original_dir: &Path,
    filename: &str,
    mime_type: &str,
    source_path: &Path,
) -> Result<OriginalFileManifest, String> {
    let filename = safe_filename(filename);
    create_dir_all(original_dir)?;
    let file_path = original_dir.join(&filename);
    let size_bytes = copy_file_atomic(source_path, &file_path).map_err(|error| {
        format!(
            "copy original file {} to {}: {error}",
            display_path(source_path),
            display_path(&file_path)
        )
    })? as usize;
    write_original_manifest(original_dir, &file_path, filename, mime_type, size_bytes)
}

pub(super) fn read_original_file_from_dir(
    original_dir: &Path,
) -> Result<(OriginalFileManifest, Vec<u8>), String> {
    let manifest = read_original_manifest(original_dir)?;
    let file_path = original_manifest_file_path(original_dir, &manifest.filename)?;
    let bytes = read_bytes(&file_path).map_err(|error| format!("read original file: {error}"))?;
    Ok((manifest, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_original_dir() -> PathBuf {
        std::env::temp_dir().join(format!("sophia-original-file-{}", Uuid::new_v4()))
    }

    #[test]
    fn original_bytes_write_manifest_and_preserve_created_at() {
        let original_dir = temp_original_dir();

        let first =
            save_original_bytes_to_dir(&original_dir, "../Unsafe Name.txt", "", b"hello").unwrap();
        assert_eq!(first.filename, ".._Unsafe Name.txt");
        assert_eq!(first.mime_type, "application/octet-stream");
        assert_eq!(first.size_bytes, 5);
        assert!(Path::new(&first.local_path).is_file());

        let second =
            save_original_bytes_to_dir(&original_dir, "renamed.md", "text/markdown", b"# title\n")
                .unwrap();
        assert_eq!(second.created_at, first.created_at);
        assert_eq!(second.filename, "renamed.md");
        assert_eq!(second.mime_type, "text/markdown");
        assert_eq!(second.size_bytes, 8);

        let (read_manifest, bytes) = read_original_file_from_dir(&original_dir).unwrap();
        assert_eq!(read_manifest.filename, "renamed.md");
        assert_eq!(bytes, b"# title\n");

        fs::remove_dir_all(original_dir).unwrap();
    }

    #[test]
    fn original_file_base64_decode_errors_are_contextual() {
        let original_dir = temp_original_dir();
        let error =
            save_original_file_to_dir(&original_dir, "bad.txt", "text/plain", "not base64%%")
                .unwrap_err();
        assert!(error.contains("decode original file"));
        assert!(!original_dir.exists());
    }

    #[test]
    fn original_file_from_path_copies_without_requiring_inline_bytes() {
        let root = temp_original_dir();
        let source_path = root.join("source.bin");
        let original_dir = root.join("original");
        fs::create_dir_all(&root).unwrap();
        fs::write(&source_path, b"streamed image").unwrap();

        let first = save_original_file_from_path_to_dir(
            &original_dir,
            "../Image.png",
            "image/png",
            &source_path,
        )
        .unwrap();

        assert_eq!(first.filename, ".._Image.png");
        assert_eq!(first.mime_type, "image/png");
        assert_eq!(first.size_bytes, 14);
        assert_eq!(
            fs::read(original_dir.join(".._Image.png")).unwrap(),
            b"streamed image"
        );

        fs::write(&source_path, b"updated").unwrap();
        let second =
            save_original_file_from_path_to_dir(&original_dir, "updated.png", "", &source_path)
                .unwrap();

        assert_eq!(second.created_at, first.created_at);
        assert_eq!(second.filename, "updated.png");
        assert_eq!(second.mime_type, "application/octet-stream");
        assert_eq!(second.size_bytes, 7);
        assert_eq!(
            fs::read(original_dir.join("updated.png")).unwrap(),
            b"updated"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn original_file_read_rejects_unsafe_manifest_filename() {
        let original_dir = temp_original_dir();
        fs::create_dir_all(&original_dir).unwrap();
        write_json(
            &original_dir.join("manifest.json"),
            &OriginalFileManifest {
                source_filename: None,
                filename: "../outside.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size_bytes: 5,
                local_path: "/tmp/outside.txt".to_string(),
                created_at: "1".to_string(),
                updated_at: "2".to_string(),
            },
        )
        .unwrap();

        let error = read_original_file_from_dir(&original_dir).unwrap_err();

        assert!(error.contains("unsafe path characters"));

        write_json(
            &original_dir.join("manifest.json"),
            &OriginalFileManifest {
                source_filename: None,
                filename: "..".to_string(),
                mime_type: "text/plain".to_string(),
                size_bytes: 5,
                local_path: "/tmp/outside.txt".to_string(),
                created_at: "1".to_string(),
                updated_at: "2".to_string(),
            },
        )
        .unwrap();
        let error = read_original_file_from_dir(&original_dir).unwrap_err();

        assert!(error.contains("unsafe path characters"));
        fs::remove_dir_all(original_dir).unwrap();
    }

    #[test]
    fn rewrite_manifest_repairs_copied_local_path() {
        let source_root = temp_original_dir();
        let original_dir = source_root.join("original");
        let manifest =
            save_original_bytes_to_dir(&original_dir, "original.txt", "text/plain", b"before move")
                .unwrap();
        let stale_path = manifest.local_path;
        let moved_root = temp_original_dir();
        fs::rename(&source_root, &moved_root).unwrap();
        let moved_original_dir = moved_root.join("original");

        rewrite_original_manifest_local_path(&moved_original_dir).unwrap();
        let repaired =
            read_json::<OriginalFileManifest>(&moved_original_dir.join("manifest.json")).unwrap();
        assert_ne!(repaired.local_path, stale_path);
        assert_eq!(
            repaired.local_path,
            display_path(&moved_original_dir.join("original.txt"))
        );

        fs::remove_dir_all(moved_root).unwrap();
    }
}
