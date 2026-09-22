use fs4::fs_std::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Advisory exclusive lock on a profile data directory. Held for the lifetime
/// of the value; released when the inner file handle is dropped (the OS
/// releases the kernel-level lock on close).
///
/// The lockfile path is `<profile_dir>/.lock`. Best-effort cleanup on Drop
/// removes the path so a clean shutdown leaves no stale entry, but the lock
/// itself is the kernel advisory state on the open file descriptor — not the
/// presence of the file.
#[derive(Debug)]
pub(crate) struct ProfileLock {
    file: Option<File>,
    path: PathBuf,
}

impl ProfileLock {
    pub(crate) fn acquire(profile_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(profile_dir).map_err(|error| {
            format!(
                "create profile dir {}: {error}",
                profile_dir.to_string_lossy()
            )
        })?;
        let path = profile_dir.join(".lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("open lockfile {}: {error}", path.to_string_lossy()))?;
        match file.try_lock_exclusive() {
            Ok(true) => Ok(Self {
                file: Some(file),
                path,
            }),
            Ok(false) => Err(format!(
                "another Sophia Native instance appears to be running on {}",
                profile_dir.to_string_lossy()
            )),
            Err(error) => Err(format!(
                "acquire lock on {}: {error}",
                path.to_string_lossy()
            )),
        }
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // Best-effort: drop the handle to release the kernel advisory
            // lock, then remove the lockfile path so a clean shutdown leaves
            // no stale entry. A crash/abnormal exit may leave the path
            // behind; that's fine — the lock state lives in the file
            // descriptor, not in the path's existence.
            let _ = FileExt::unlock(&file);
            drop(file);
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProfileLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-profile-lock-{name}-{suffix}"))
    }

    #[test]
    fn acquire_creates_lockfile_and_releases_on_drop() {
        let dir = unique_dir("acquire");
        let lock = ProfileLock::acquire(&dir).expect("first acquire should succeed");
        assert!(dir.join(".lock").exists(), "lockfile should be created");
        drop(lock);
        // After drop, a fresh acquire on the same dir must succeed.
        let lock = ProfileLock::acquire(&dir).expect("re-acquire after drop should succeed");
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_acquire_on_held_lock_is_rejected() {
        let dir = unique_dir("contention");
        let _held = ProfileLock::acquire(&dir).expect("first acquire should succeed");
        let error = ProfileLock::acquire(&dir).expect_err(
            "second acquire on a held profile dir must fail rather than silently corrupt state",
        );
        assert!(
            error.contains("another Sophia Native instance"),
            "error must identify the contention cause: {error}"
        );
        drop(_held);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
