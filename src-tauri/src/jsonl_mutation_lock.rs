//! Cross-process mutation serialization for append-only JSONL files.
//!
//! Appends and prune rewrites must lock a stable inode distinct from the data
//! file: prune atomically replaces the JSONL path, so locking the data file
//! itself would silently stop excluding writers after the rename. The sibling
//! hidden lock tree survives that replacement and `fs4` releases its advisory
//! lock automatically when a process exits or crashes.

use fs4::fs_std::FileExt;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{
        mpsc::{Receiver, Sender},
        Mutex, OnceLock,
    },
    time::Duration,
};

pub(crate) struct JsonlMutationGuard {
    _file: File,
}

pub(crate) fn jsonl_mutation_lock_path(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("JSONL path has no parent: {}", path.display()))?;
    let filename = path
        .file_name()
        .ok_or_else(|| format!("JSONL path has no filename: {}", path.display()))?;
    Ok(parent.join(".jsonl-locks").join(filename).join(".lock"))
}

pub(crate) fn acquire_jsonl_mutation_lock(path: &Path) -> Result<JsonlMutationGuard, String> {
    let lock_path = jsonl_mutation_lock_path(path)?;
    let parent = lock_path
        .parent()
        .ok_or_else(|| format!("JSONL lock has no parent: {}", lock_path.display()))?;
    crate::storage::create_dir_all(parent).map_err(|error| error.message())?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("open JSONL lock {}: {error}", lock_path.display()))?;
    file.lock_exclusive()
        .map_err(|error| format!("lock JSONL {}: {error}", lock_path.display()))?;
    #[cfg(test)]
    hold_lock_for_test_if_requested(&lock_path)?;
    Ok(JsonlMutationGuard { _file: file })
}

#[cfg(test)]
struct JsonlLockHoldHook {
    reached: Sender<PathBuf>,
    resume: Receiver<()>,
}

#[cfg(test)]
static JSONL_LOCK_HOLD_HOOKS: OnceLock<Mutex<HashMap<PathBuf, JsonlLockHoldHook>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn install_jsonl_lock_hold_hook_for_test(
    data_path: &Path,
    reached: Sender<PathBuf>,
    resume: Receiver<()>,
) -> Result<(), String> {
    let lock_path = jsonl_mutation_lock_path(data_path)?;
    JSONL_LOCK_HOLD_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(lock_path, JsonlLockHoldHook { reached, resume });
    Ok(())
}

#[cfg(test)]
pub(crate) fn clear_jsonl_lock_hold_hook_for_test(data_path: &Path) {
    let Ok(lock_path) = jsonl_mutation_lock_path(data_path) else {
        return;
    };
    JSONL_LOCK_HOLD_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&lock_path);
}

#[cfg(test)]
fn hold_lock_for_test_if_requested(lock_path: &Path) -> Result<(), String> {
    let hook = JSONL_LOCK_HOLD_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(lock_path);
    let Some(hook) = hook else {
        return Ok(());
    };
    hook.reached
        .send(lock_path.to_path_buf())
        .map_err(|error| format!("signal held JSONL lock {}: {error}", lock_path.display()))?;
    hook.resume
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| format!("resume held JSONL lock {}: {error}", lock_path.display()))?;
    Ok(())
}
