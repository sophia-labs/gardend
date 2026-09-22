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
    fs::read(path)
        .map_err(|error| AppError::storage(format!("read {}: {error}", display_path(path))))
}

pub fn write_bytes(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let temp_path = atomic_temp_path(path);
    let result = write_bytes_atomic(path, &temp_path, bytes, None).map_err(AppError::storage);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub fn copy_file_atomic(source: &Path, target: &Path) -> AppResult<u64> {
    if let Some(parent) = target.parent() {
        create_dir_all(parent)?;
    }

    let temp_path = atomic_temp_path(target);
    let result = copy_file_atomic_inner(source, target, &temp_path).map_err(AppError::storage);
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
        AppError::storage(format!("remove directory {}: {error}", display_path(path)))
    })?;
    if let Some(parent) = path.parent() {
        sync_parent_dir(parent).map_err(AppError::storage)?;
    }
    Ok(())
}

fn remove_file_inner(path: &Path, allow_missing: bool) -> AppResult<bool> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_parent_dir(parent).map_err(AppError::storage)?;
            }
            Ok(true)
        }
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::storage(format!(
            "remove {}: {error}",
            display_path(path)
        ))),
    }
}

pub(crate) fn write_secret_bytes(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let temp_path = atomic_temp_path(path);
    let result =
        write_bytes_atomic(path, &temp_path, bytes, Some(0o600)).map_err(AppError::storage);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub fn append_secret_line(path: &Path, line: &str) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| AppError::storage(format!("open {}: {error}", display_path(path))))?
    };

    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| AppError::storage(format!("open {}: {error}", display_path(path))))?;

    // Keep one logical JSONL record in one append buffer. In particular, do
    // not expose a newline-less JSON write that another O_APPEND writer can
    // interleave with before the terminator is written.
    let mut record = Vec::with_capacity(line.len().saturating_add(1));
    record.extend_from_slice(line.as_bytes());
    record.push(b'\n');
    file.write_all(&record)
        .and_then(|_| file.sync_all())
        .map_err(|error| AppError::storage(format!("append {}: {error}", display_path(path))))?;
    Ok(())
}

pub fn create_dir_all(path: &Path) -> AppResult<()> {
    fs::create_dir_all(path)
        .map_err(|error| AppError::storage(format!("create {}: {error}", display_path(path))))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            sync_parent_dir(parent).map_err(AppError::storage)?;
        }
    }
    Ok(())
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
