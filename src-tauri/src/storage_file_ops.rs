use crate::{
    app_error::{AppError, AppResult},
    storage_atomic::{
        atomic_temp_path, copy_file_atomic_inner, display_path, sync_parent_dir, write_bytes_atomic,
    },
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

pub fn read_bytes(path: &Path) -> AppResult<Vec<u8>> {
    fs::read(path).map_err(|error| {
        AppError::from(crate::app_error::PersistenceError::io(
            error,
            format!("read {}", display_path(path)),
        ))
    })
}

pub fn write_bytes(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let temp_path = atomic_temp_path(path);
    let result = write_bytes_atomic(path, &temp_path, bytes, None).map_err(AppError::from);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// [`write_bytes`], skipped when `path` already holds exactly `bytes`.
///
/// The comparison is against the file itself (its length, then its content), not
/// against a remembered hash: a file edited from outside the process is rewritten,
/// and the check needs no state that could go stale. A skipped write leaves the
/// existing file, which an earlier atomic write already synced, untouched. Returns
/// whether the file was written.
pub fn write_bytes_if_changed(path: &Path, bytes: &[u8]) -> AppResult<bool> {
    if file_holds_exactly(path, bytes) {
        tracing::info!(target: "gardend_otel", { sophia.files.skipped_unchanged = 1_u64, sophia.files.bytes = bytes.len() as u64 }, "file unchanged");
        return Ok(false);
    }
    write_bytes(path, bytes)?;
    Ok(true)
}

fn file_holds_exactly(path: &Path, bytes: &[u8]) -> bool {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() == bytes.len() as u64 => {}
        _ => return false,
    }
    fs::read(path).is_ok_and(|existing| existing == bytes)
}

pub fn copy_file_atomic(source: &Path, target: &Path) -> AppResult<u64> {
    if let Some(parent) = target.parent() {
        create_dir_all(parent)?;
    }

    let temp_path = atomic_temp_path(target);
    let result = copy_file_atomic_inner(source, target, &temp_path).map_err(AppError::from);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
pub fn remove_file(path: &Path) -> AppResult<()> {
    remove_file_inner(path, false).map(|_| ())
}

pub fn remove_file_if_exists(path: &Path) -> AppResult<bool> {
    remove_file_inner(path, true)
}

pub fn remove_dir_all(path: &Path) -> AppResult<()> {
    fs::remove_dir_all(path).map_err(|error| {
        AppError::from(crate::app_error::PersistenceError::io(
            error,
            format!("remove directory {}", display_path(path)),
        ))
    })?;
    if let Some(parent) = path.parent() {
        sync_parent_dir(parent).map_err(AppError::from)?;
    }
    Ok(())
}

fn remove_file_inner(path: &Path, allow_missing: bool) -> AppResult<bool> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_parent_dir(parent).map_err(AppError::from)?;
            }
            Ok(true)
        }
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::from(crate::app_error::PersistenceError::io(
            error,
            format!("remove {}", display_path(path)),
        ))),
    }
}

pub(crate) fn write_secret_bytes(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let temp_path = atomic_temp_path(path);
    let result = write_bytes_atomic(path, &temp_path, bytes, Some(0o600)).map_err(AppError::from);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Append one JSONL record and force it to disk.
pub fn append_secret_line(path: &Path, line: &str) -> AppResult<()> {
    append_line(path, line, true)
}

/// Append one JSONL record WITHOUT forcing it: the bytes are in the page cache when
/// this returns, so a process crash loses nothing, and a power loss may lose the
/// last records. For logs that are telemetry, or whose loss a replay ledger covers.
pub fn append_secret_line_unsynced(path: &Path, line: &str) -> AppResult<()> {
    append_line(path, line, false)
}

fn append_line(path: &Path, line: &str, sync: bool) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    // Read access too: the tail byte is checked below.
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| {
                AppError::from(crate::app_error::PersistenceError::io(
                    error,
                    format!("open {}", display_path(path)),
                ))
            })?
    };

    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| {
            AppError::from(crate::app_error::PersistenceError::io(
                error,
                format!("open {}", display_path(path)),
            ))
        })?;

    // Keep one logical JSONL record in one append buffer. In particular, do
    // not expose a newline-less JSON write that another O_APPEND writer can
    // interleave with before the terminator is written.
    let mut record = Vec::with_capacity(line.len().saturating_add(2));
    // A record torn by a crash leaves a tail without its newline; appending right
    // after it would fuse the new record into the torn one and lose both. Start the
    // new record on its own line instead.
    if tail_is_unterminated(&file) {
        record.push(b'\n');
    }
    record.extend_from_slice(line.as_bytes());
    record.push(b'\n');
    file.write_all(&record)
        .and_then(|_| {
            if sync {
                crate::storage_atomic::traced_sync(&file, "file")
            } else {
                Ok(())
            }
        })
        .map_err(|error| {
            AppError::from(crate::app_error::PersistenceError::io(
                error,
                format!("append {}", display_path(path)),
            ))
        })?;
    Ok(())
}

/// Whether the file is non-empty and its last byte is not a newline.
#[cfg(unix)]
fn tail_is_unterminated(file: &fs::File) -> bool {
    use std::os::unix::fs::FileExt;
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return false;
    };
    if length == 0 {
        return false;
    }
    let mut last = [0u8; 1];
    match file.read_exact_at(&mut last, length - 1) {
        Ok(()) => last[0] != b'\n',
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn tail_is_unterminated(_file: &fs::File) -> bool {
    false
}

/// Persist newly created directory entries, including missing ancestors.
/// Graph lookups call this to heal their layout on reads; an existing directory
/// changes nothing and must not force a full-drive flush on macOS.
pub fn create_dir_all(path: &Path) -> AppResult<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    let result = match fs::create_dir(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                create_dir_all(parent)?;
            }
            fs::create_dir(path)
        }
        result => result,
    };
    match result {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                // A single-component relative path has an empty parent: its
                // new entry belongs to the current directory.
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                sync_parent_dir(parent).map_err(AppError::from)?;
            }
            Ok(())
        }
        // Also accepts a directory made by a concurrent creator, which owns
        // the sync of its new entry, just as we own the successful mkdir above.
        Err(_) if path.is_dir() => Ok(()),
        Err(error) => Err(AppError::from(crate::app_error::PersistenceError::io(
            error,
            format!("create {}", display_path(path)),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_path(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-storage-{name}-{suffix}.json"))
    }

    /// W3: the unsynced append writes the same bytes without forcing the file; the
    /// forced one still forces it exactly once. (Syncs are counted per thread.)
    #[test]
    fn unsynced_append_writes_the_record_without_a_sync() {
        let root = temp_test_path("append").with_extension("");
        let path = root.join("log.jsonl");
        let before = crate::storage_atomic::thread_sync_count("file");
        append_secret_line_unsynced(&path, r#"{"n":1}"#).expect("unsynced append");
        append_secret_line_unsynced(&path, r#"{"n":2}"#).expect("unsynced append");
        assert_eq!(
            crate::storage_atomic::thread_sync_count("file"),
            before,
            "no sync for an unsynced append"
        );
        append_secret_line(&path, r#"{"n":3}"#).expect("forced append");
        assert_eq!(
            crate::storage_atomic::thread_sync_count("file"),
            before + 1,
            "the forced append syncs once"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read log"),
            "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// A record torn by a crash leaves a tail with no newline. The next append
    /// starts its own line, so the torn fragment costs one unreadable line and
    /// not the record after it.
    #[test]
    fn append_after_a_torn_tail_starts_a_fresh_line() {
        let root = temp_test_path("torn").with_extension("");
        let path = root.join("log.jsonl");
        create_dir_all(&root).expect("root");
        fs::write(&path, "{\"n\":1}\n{\"n\":2,\"tor").expect("torn tail");
        append_secret_line_unsynced(&path, r#"{"n":3}"#).expect("append after tear");
        let raw = fs::read_to_string(&path).expect("read log");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(serde_json::from_str::<serde_json::Value>(lines[1]).is_err());
        assert_eq!(lines[2], r#"{"n":3}"#, "the new record is intact on its own line");
        // A clean tail gets no blank line.
        append_secret_line(&path, r#"{"n":4}"#).expect("append after a clean tail");
        assert_eq!(
            fs::read_to_string(&path).expect("read log").lines().count(),
            4
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn write_bytes_creates_parent_and_overwrites() {
        let root = temp_test_path("bytes").with_extension("");
        let path = root.join("nested").join("blob.bin");
        write_bytes(&path, b"first").expect("write first bytes");
        write_bytes(&path, b"second").expect("write second bytes");
        let bytes = read_bytes(&path).expect("read bytes");
        assert_eq!(bytes, b"second");
        let leftovers = fs::read_dir(path.parent().expect("parent"))
            .expect("read parent")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn create_dir_all_creates_nested_directory() {
        let root = temp_test_path("mkdir").with_extension("");
        let path = root.join("nested").join("jobs");

        create_dir_all(&path).expect("create directory");

        assert!(path.is_dir());
        create_dir_all(&path).expect("create existing directory");
        assert!(path.is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn create_dir_all_refuses_file_at_target_or_ancestor() {
        let root = temp_test_path("mkdir-file").with_extension("");
        let path = root.join("file");
        write_bytes(&path, b"keep").expect("write blocking file");

        assert!(create_dir_all(&path).is_err());
        assert!(create_dir_all(&path.join("child")).is_err());
        assert_eq!(read_bytes(&path).expect("read blocking file"), b"keep");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copy_file_atomic_creates_parent_and_overwrites() {
        let root = temp_test_path("copy").with_extension("");
        let source = root.join("source.bin");
        let target = root.join("nested").join("target.bin");
        write_bytes(&source, b"first").expect("write source bytes");
        write_bytes(&target, b"stale").expect("write stale target bytes");

        let bytes_copied = copy_file_atomic(&source, &target).expect("copy file");

        assert_eq!(bytes_copied, 5);
        let bytes = read_bytes(&target).expect("read target bytes");
        assert_eq!(bytes, b"first");
        let leftovers = fs::read_dir(target.parent().expect("parent"))
            .expect("read parent")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_file_helpers_remove_and_tolerate_missing() {
        let root = temp_test_path("remove").with_extension("");
        let path = root.join("nested").join("stale.bin");
        write_bytes(&path, b"stale").expect("write stale bytes");

        assert!(remove_file_if_exists(&path).expect("remove existing file"));
        assert!(!path.exists());
        assert!(!remove_file_if_exists(&path).expect("ignore missing file"));

        let missing = root.join("missing.bin");
        let error = remove_file(&missing).expect_err("missing remove should error");
        assert!(error.to_string().contains("remove "));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_dir_all_removes_nested_directory() {
        let root = temp_test_path("remove-dir").with_extension("");
        let path = root.join("nested").join("stale.bin");
        write_bytes(&path, b"stale").expect("write stale bytes");

        remove_dir_all(&root).expect("remove directory tree");

        assert!(!root.exists());
        let error = remove_dir_all(&root).expect_err("missing directory should error");
        assert!(error.to_string().contains("remove directory "));
    }

    #[test]
    fn append_secret_line_creates_and_appends_lines() {
        let root = temp_test_path("append-secret").with_extension("");
        let path = root.join("nested").join("audit.jsonl");

        append_secret_line(&path, "{\"a\":1}").expect("append first line");
        append_secret_line(&path, "{\"b\":2}").expect("append second line");

        let raw = fs::read_to_string(&path).expect("read audit lines");
        assert_eq!(raw, "{\"a\":1}\n{\"b\":2}\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(root);
    }
}
