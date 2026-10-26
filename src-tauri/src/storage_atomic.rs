use crate::app_error::PersistenceError;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// `File::sync_all`, which on macOS is `F_FULLFSYNC`: it flushes the whole drive's cache, not just this file, so its
/// count and cost per action are the durability budget the traces need (DESIGN.md, the durability cost model). Each
/// call is a timed `fsync` event on the span that is open, labelled by what it made durable; nothing about the path.
pub(crate) fn traced_sync(file: &fs::File, what: &'static str) -> std::io::Result<()> {
    #[cfg(test)]
    THREAD_SYNCS.with(|syncs| syncs.borrow_mut().push(what));
    let start = crate::telemetry::timer();
    let result = force(file);
    if start.is_some() {
        let ms = crate::telemetry::elapsed_ms(start);
        tracing::info!(target: "gardend_otel", { sophia.fsync.what = what, sophia.fsync.ms = ms }, "fsync");
    }
    result
}

/// Syncs issued by THIS thread, by label: a lib test that calls a writer on its own thread can assert that a
/// path did, or did not, force the file. Per thread, so parallel tests cannot move each other's counts.
#[cfg(test)]
thread_local! {
    static THREAD_SYNCS: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn thread_sync_count(what: &str) -> usize {
    THREAD_SYNCS.with(|syncs| {
        syncs
            .borrow()
            .iter()
            .filter(|label| **label == what)
            .count()
    })
}

/// The flush itself. Lib tests issue a plain `fsync(2)`: it keeps the syscall and its ordering but not the drive drain,
/// which a test cannot observe. Every persist the battery runs used to pay 20 ms of F_FULLFSYNC at about 5% CPU, so the
/// gate took an hour. Production keeps `sync_all` exactly.
#[cfg(not(test))]
fn force(file: &fs::File) -> std::io::Result<()> {
    file.sync_all()
}

#[cfg(all(test, unix))]
fn force(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn fsync(fd: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    // SAFETY: the descriptor belongs to `file`, which outlives the call.
    if unsafe { fsync(file.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(test, not(unix)))]
fn force(file: &fs::File) -> std::io::Result<()> {
    file.sync_all()
}

pub(crate) fn copy_file_atomic_inner(
    source: &Path,
    target: &Path,
    temp_path: &Path,
) -> Result<u64, PersistenceError> {
    let bytes_copied = fs::copy(source, temp_path).map_err(|error| {
        PersistenceError::io(
            error,
            format!(
                "copy {} to {}",
                display_path(source),
                display_path(temp_path)
            ),
        )
    })?;
    fs::OpenOptions::new()
        .read(true)
        .open(temp_path)
        .and_then(|file| traced_sync(&file, "atomic.copy"))
        .map_err(|error| {
            PersistenceError::io(error, format!("sync {}", display_path(temp_path)))
        })?;
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
    unix_mode: Option<u32>,
) -> Result<(), PersistenceError> {
    write_bytes_atomic_with(path, temp_path, bytes, unix_mode, true)
}

/// `write_bytes_atomic` without the final parent-directory sync: the file's bytes are durable and the rename is
/// atomic, but the new directory entry is durable only after the caller syncs the directory itself. For a caller
/// that writes many files into one directory and syncs it once (restore-point bundles).
pub(crate) fn write_bytes_atomic_no_dir_sync(
    path: &Path,
    temp_path: &Path,
    bytes: &[u8],
    unix_mode: Option<u32>,
) -> Result<(), PersistenceError> {
    write_bytes_atomic_with(path, temp_path, bytes, unix_mode, false)
}

/// [`write_bytes_atomic`] with the parent-directory sync made optional. The
/// directory sync only makes a *new name* durable; replacing an existing
/// entry by rename needs none for the file to be whole after a power cut (the
/// old or the new complete file survives, never a torn one). A caller that
/// skips it must make one at its own cadence (`sync_parent_dir`).
pub(crate) fn write_bytes_atomic_with(
    path: &Path,
    temp_path: &Path,
    bytes: &[u8],
    #[cfg_attr(not(unix), allow(unused_variables))] unix_mode: Option<u32>,
    sync_dir: bool,
) -> Result<(), PersistenceError> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        if let Some(mode) = unix_mode {
            options.mode(mode);
        }
    }

    let mut file = options.open(temp_path).map_err(|error| {
        PersistenceError::io(error, format!("write {}", display_path(temp_path)))
    })?;
    file.write_all(bytes).map_err(|error| {
        PersistenceError::io(error, format!("write {}", display_path(temp_path)))
    })?;
    traced_sync(&file, "atomic.file").map_err(|error| {
        PersistenceError::io(error, format!("sync {}", display_path(temp_path)))
    })?;
    drop(file);

    rename_atomic(temp_path, path)?;
    if sync_dir {
        if let Some(parent) = path.parent() {
            sync_parent_dir(parent)?;
        }
    }
    Ok(())
}

pub(crate) fn sync_parent_dir(parent: &Path) -> Result<(), PersistenceError> {
    fs::OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|directory| traced_sync(&directory, "dir"))
        .map_err(|error| {
            PersistenceError::io(error, format!("sync parent {}", display_path(parent)))
        })
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

fn rename_atomic(temp_path: &Path, path: &Path) -> Result<(), PersistenceError> {
    match fs::rename(temp_path, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            {
                if path.exists() {
                    fs::remove_file(path).map_err(|remove_error| {
                        PersistenceError::io(
                            remove_error,
                            format!(
                                "replace {} after rename failed ({error})",
                                display_path(path)
                            ),
                        )
                    })?;
                    return fs::rename(temp_path, path).map_err(|rename_error| {
                        PersistenceError::io(
                            rename_error,
                            format!(
                                "rename {} to {}",
                                display_path(temp_path),
                                display_path(path)
                            ),
                        )
                    });
                }
            }
            Err(PersistenceError::io(
                error,
                format!(
                    "rename {} to {}",
                    display_path(temp_path),
                    display_path(path)
                ),
            ))
        }
    }
}
