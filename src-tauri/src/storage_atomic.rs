use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub(crate) fn copy_file_atomic_inner(
    source: &Path,
    target: &Path,
    temp_path: &Path,
) -> Result<u64, String> {
    let bytes_copied = fs::copy(source, temp_path).map_err(|error| {
        format!(
            "copy {} to {}: {error}",
            display_path(source),
            display_path(temp_path)
        )
    })?;
    fs::OpenOptions::new()
        .read(true)
        .open(temp_path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync {}: {error}", display_path(temp_path)))?;
    rename_atomic(temp_path, target)?;
    if let Some(parent) = target.parent() {
        sync_parent_dir(parent)?;
    }
    Ok(bytes_copied)
}

pub(crate) fn write_bytes_atomic(
    path: &Path,
    temp_path: &Path,
    bytes: &[u8],
    #[cfg_attr(not(unix), allow(unused_variables))] unix_mode: Option<u32>,
) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        if let Some(mode) = unix_mode {
            options.mode(mode);
        }
    }

    let mut file = options
        .open(temp_path)
        .map_err(|error| format!("write {}: {error}", display_path(temp_path)))?;
    file.write_all(bytes)
        .map_err(|error| format!("write {}: {error}", display_path(temp_path)))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", display_path(temp_path)))?;
    drop(file);

    rename_atomic(temp_path, path)?;
    if let Some(parent) = path.parent() {
        sync_parent_dir(parent)?;
    }
    Ok(())
}

pub(crate) fn sync_parent_dir(parent: &Path) -> Result<(), String> {
    fs::OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync parent {}: {error}", display_path(parent)))
}

pub(crate) fn atomic_temp_path(path: &Path) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("tmp");
    path.with_file_name(format!(".{filename}.tmp-{}-{suffix}", std::process::id()))
}

pub fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::sync_parent_dir;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-atomic-{name}-{suffix}"))
    }

    #[test]
    fn sync_parent_dir_succeeds_on_real_directory() {
        let dir = unique_dir("ok");
        std::fs::create_dir_all(&dir).expect("create dir");
        sync_parent_dir(&dir).expect("sync existing directory");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_parent_dir_surfaces_error_when_missing() {
        let dir = unique_dir("missing");
        let error = sync_parent_dir(&dir)
            .expect_err("sync_parent_dir on missing path must return an error, not swallow it");
        assert!(
            error.contains("sync parent"),
            "error should identify the failing operation: {error}"
        );
    }
}

fn rename_atomic(temp_path: &Path, path: &Path) -> Result<(), String> {
    match fs::rename(temp_path, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            {
                if path.exists() {
                    fs::remove_file(path).map_err(|remove_error| {
                        format!(
                            "replace {} after rename failed ({error}): {remove_error}",
                            display_path(path)
                        )
                    })?;
                    return fs::rename(temp_path, path).map_err(|rename_error| {
                        format!(
                            "rename {} to {}: {rename_error}",
                            display_path(temp_path),
                            display_path(path)
                        )
                    });
                }
            }
            Err(format!(
                "rename {} to {}: {error}",
                display_path(temp_path),
                display_path(path)
            ))
        }
    }
}
