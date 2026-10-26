use crate::app_error::PersistenceError;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// How hard a persist is forced.
///
/// `Full` is `File::sync_all`, which on macOS is `F_FULLFSYNC`: it flushes the whole drive's cache, 10-15 ms a call,
/// and it is what every durability point (shutdown, restore-point completion, keys, migrations, replace-the-old-copy)
/// keeps. `Ordered` is for authority and projection persists that sit inside the ratified durability window (a power
/// loss may cost up to ~1 s of acknowledged edits, never corruption): on macOS it is `F_BARRIERFSYNC`, which orders
/// this file's writes on the media without draining the drive cache, and it owes the drive one full flush within
/// [`FULL_FLUSH_PERIOD`] (see [`flush_now`] and the flusher thread). Everywhere else `Ordered` is `Full`: Linux, which
/// runs the cells, is not weakened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Durability {
    Full,
    Ordered,
}

/// What a sync call actually issued, for the `fsync` telemetry event and the tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncKind {
    Barrier,
    Full,
}

impl SyncKind {
    fn as_str(self) -> &'static str {
        match self {
            SyncKind::Barrier => "barrier",
            SyncKind::Full => "full",
        }
    }
}

/// The longest a barrier-only persist waits for its full flush: the flusher ticks at this period, and a persist
/// that lands just after a tick waits one period (plus the flush itself). Under Vera's 1 s ruling with margin.
pub(crate) const FULL_FLUSH_PERIOD: std::time::Duration = std::time::Duration::from_millis(500);

/// A full-strength sync (`F_FULLFSYNC` on macOS). Each call is a timed `fsync` event on the span that is open,
/// labelled by what it made durable and what kind it was; nothing about the path.
pub(crate) fn traced_sync(file: &fs::File, what: &'static str) -> std::io::Result<()> {
    traced_sync_with(file, what, Durability::Full)
}

pub(crate) fn traced_sync_with(
    file: &fs::File,
    what: &'static str,
    durability: Durability,
) -> std::io::Result<()> {
    let start = crate::telemetry::timer();
    let (result, kind) = match durability {
        Durability::Full => (force(file), SyncKind::Full),
        Durability::Ordered => force_ordered(file),
    };
    #[cfg(test)]
    THREAD_SYNCS.with(|syncs| syncs.borrow_mut().push((what, kind)));
    if start.is_some() {
        let ms = crate::telemetry::elapsed_ms(start);
        let kind = kind.as_str();
        tracing::info!(target: "gardend_otel", { sophia.fsync.what = what, sophia.fsync.kind = kind, sophia.fsync.ms = ms }, "fsync");
    }
    result
}

/// Syncs issued by THIS thread, by label and kind: a lib test that calls a writer on its own thread can assert that
/// a path did, or did not, force the file. Per thread, so parallel tests cannot move each other's counts.
#[cfg(test)]
thread_local! {
    static THREAD_SYNCS: std::cell::RefCell<Vec<(&'static str, SyncKind)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Test override of "this platform has barriers": `None` is the native answer.
    static BARRIER_MODEL: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Syncs of any kind labelled `what` on this thread.
#[cfg(test)]
pub(crate) fn thread_sync_count(what: &str) -> usize {
    THREAD_SYNCS.with(|syncs| syncs.borrow().iter().filter(|(label, _)| *label == what).count())
}

/// Syncs of one kind labelled `what` on this thread.
#[cfg(test)]
pub(crate) fn thread_sync_count_kind(what: &str, kind: SyncKind) -> usize {
    THREAD_SYNCS.with(|syncs| {
        syncs
            .borrow()
            .iter()
            .filter(|(label, k)| *label == what && *k == kind)
            .count()
    })
}

/// Every full-strength sync on this thread, whatever its label.
#[cfg(test)]
pub(crate) fn thread_full_sync_total() -> usize {
    THREAD_SYNCS.with(|syncs| {
        syncs
            .borrow()
            .iter()
            .filter(|(_, kind)| *kind == SyncKind::Full)
            .count()
    })
}

/// Make this thread behave as if the platform has (or lacks) write barriers, so the macOS path is testable
/// anywhere. The syscall under it is still a plain `fsync(2)`.
#[cfg(test)]
pub(crate) fn set_thread_barrier_model(has_barriers: bool) {
    BARRIER_MODEL.with(|model| model.set(Some(has_barriers)));
}

#[cfg(not(test))]
fn barrier_model() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(test)]
fn barrier_model() -> bool {
    BARRIER_MODEL
        .with(|model| model.get())
        .unwrap_or(cfg!(target_os = "macos"))
}

/// `Ordered` for one file: a barrier where there are barriers (and the file's device is noted as owing a full
/// flush), else a full sync. A barrier the OS refuses falls back to the full sync, never to nothing.
fn force_ordered(file: &fs::File) -> (std::io::Result<()>, SyncKind) {
    if barrier_model() && barrier(file).is_ok() {
        note_full_flush_owed(file);
        return (Ok(()), SyncKind::Barrier);
    }
    (force(file), SyncKind::Full)
}

/// `F_BARRIERFSYNC` (`fcntl(fd, 85)`): this file's writes reach the media before any later write does, with no
/// drive-cache drain.
#[cfg(all(not(test), target_os = "macos"))]
fn barrier(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn fcntl(fd: std::os::raw::c_int, cmd: std::os::raw::c_int, ...) -> std::os::raw::c_int;
    }
    const F_BARRIERFSYNC: std::os::raw::c_int = 85;
    // SAFETY: the descriptor belongs to `file`, which outlives the call; F_BARRIERFSYNC takes no argument.
    if unsafe { fcntl(file.as_raw_fd(), F_BARRIERFSYNC) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Where there is no barrier call (or in lib tests, which only keep the syscall and its ordering): a plain sync.
#[cfg(not(all(not(test), target_os = "macos")))]
fn barrier(file: &fs::File) -> std::io::Result<()> {
    force(file)
}

// ---- the owed full flush -------------------------------------------------------------------------------------

/// Bumped (after the descriptor is registered) by every barrier-only sync; `FLUSHED_GEN` is the newest generation a
/// full flush has covered. A flush is owed while the first is ahead of the second.
static OWED_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FLUSHED_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One descriptor per device that owes a flush: the newest file barriered on it. A full sync of any file on a
/// device drains that device's cache, so one descriptor per device is enough. Holding the descriptor (a `dup`) keeps
/// it valid even if the file was since renamed or replaced.
static OWED_FDS: std::sync::Mutex<Vec<(u64, fs::File)>> = std::sync::Mutex::new(Vec::new());
/// Serializes flushers, so a `flush_now` that finds nothing to take still waits for the flush in progress.
static FLUSH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static FLUSHER: std::sync::Once = std::sync::Once::new();

#[cfg(unix)]
fn device_of(file: &fs::File) -> u64 {
    use std::os::unix::fs::MetadataExt;
    file.metadata().map(|metadata| metadata.dev()).unwrap_or(0)
}

#[cfg(not(unix))]
fn device_of(_file: &fs::File) -> u64 {
    0
}

fn note_full_flush_owed(file: &fs::File) {
    if let Ok(held) = file.try_clone() {
        let device = device_of(file);
        let mut owed = OWED_FDS.lock().unwrap_or_else(|e| e.into_inner());
        match owed.iter_mut().find(|(dev, _)| *dev == device) {
            Some(slot) => slot.1 = held,
            None => owed.push((device, held)),
        }
        OWED_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    } else {
        // No descriptor to flush through later: pay for the full sync now.
        let _ = force(file);
        return;
    }
    FLUSHER.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("storage-full-flush".into())
            .spawn(|| loop {
                std::thread::sleep(FULL_FLUSH_PERIOD);
                if let Err(error) = flush_now() {
                    log::warn!("owed full flush failed (will retry): {error}");
                }
            });
        if let Err(error) = spawned {
            log::error!("could not start the full-flush thread: {error}");
        }
    });
}

#[cfg(test)]
pub(crate) fn owed_generation() -> u64 {
    OWED_GEN.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn flushed_generation() -> u64 {
    FLUSHED_GEN.load(std::sync::atomic::Ordering::SeqCst)
}

/// Pay every owed full flush now, synchronously: a durability point (shutdown, restore-point completion, the
/// explicit flush tool) calls this after its ordered writes. When nothing is owed it returns at once, but it still
/// waits for a flush already in progress, so on return everything barriered before the call is on the media. On
/// failure the owed descriptors go back, so the flusher retries.
pub(crate) fn flush_now() -> std::io::Result<()> {
    use std::sync::atomic::Ordering::SeqCst;
    let _flushing = FLUSH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (generation, taken) = {
        let mut owed = OWED_FDS.lock().unwrap_or_else(|e| e.into_inner());
        (OWED_GEN.load(SeqCst), std::mem::take(&mut *owed))
    };
    let mut failure = None;
    let mut unflushed = Vec::new();
    for (device, file) in taken {
        match traced_sync(&file, "flush") {
            Ok(()) => {}
            Err(error) => {
                failure.get_or_insert(error);
                unflushed.push((device, file));
            }
        }
    }
    if let Some(error) = failure {
        let mut owed = OWED_FDS.lock().unwrap_or_else(|e| e.into_inner());
        for (device, file) in unflushed {
            if !owed.iter().any(|(dev, _)| *dev == device) {
                owed.push((device, file));
            }
        }
        return Err(error);
    }
    FLUSHED_GEN.fetch_max(generation, SeqCst);
    Ok(())
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

/// [`write_bytes_atomic`] with the parent-directory sync made optional. The
/// directory sync only makes a *new name* durable; replacing an existing
/// entry by rename needs none for the file to be whole after a power cut (the
/// old or the new complete file survives, never a torn one). A caller that
/// skips it must make one at its own cadence (`sync_parent_dir`).
pub(crate) fn write_bytes_atomic_with(
    path: &Path,
    temp_path: &Path,
    bytes: &[u8],
    unix_mode: Option<u32>,
    sync_dir: bool,
) -> Result<(), PersistenceError> {
    write_bytes_atomic_durable(path, temp_path, bytes, unix_mode, sync_dir, Durability::Full)
}

/// [`write_bytes_atomic_with`] at a chosen [`Durability`]. The order never changes: write the temp file, sync it,
/// rename, then sync the directory (when the entry changed). `Ordered` only softens each sync to a barrier.
pub(crate) fn write_bytes_atomic_durable(
    path: &Path,
    temp_path: &Path,
    bytes: &[u8],
    #[cfg_attr(not(unix), allow(unused_variables))] unix_mode: Option<u32>,
    sync_dir: bool,
    durability: Durability,
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
    traced_sync_with(&file, "atomic.file", durability).map_err(|error| {
        PersistenceError::io(error, format!("sync {}", display_path(temp_path)))
    })?;
    drop(file);

    rename_atomic(temp_path, path)?;
    if sync_dir {
        if let Some(parent) = path.parent() {
            sync_parent_dir_with(parent, durability)?;
        }
    }
    Ok(())
}

pub(crate) fn sync_parent_dir(parent: &Path) -> Result<(), PersistenceError> {
    sync_parent_dir_with(parent, Durability::Full)
}

pub(crate) fn sync_parent_dir_with(
    parent: &Path,
    durability: Durability,
) -> Result<(), PersistenceError> {
    fs::OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|directory| traced_sync_with(&directory, "dir", durability))
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
    use super::{
        atomic_temp_path, flush_now, flushed_generation, owed_generation,
        set_thread_barrier_model, sync_parent_dir, thread_full_sync_total, thread_sync_count_kind,
        traced_sync, write_bytes_atomic, write_bytes_atomic_durable, Durability, SyncKind,
        FULL_FLUSH_PERIOD,
    };
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-atomic-{name}-{suffix}"))
    }


    fn ordered_write(dir: &std::path::Path, name: &str, durability: Durability) {
        let path = dir.join(name);
        let temp = atomic_temp_path(&path);
        write_bytes_atomic_durable(&path, &temp, b"bytes", None, true, durability)
            .expect("atomic write");
        assert_eq!(std::fs::read(&path).expect("read back"), b"bytes");
    }

    /// An ordered persist on a platform with barriers issues barriers only (file, then directory), no full
    /// flush on the writing thread, and owes a full flush.
    #[test]
    fn an_ordered_persist_issues_barriers_not_full_flushes_and_owes_a_flush() {
        set_thread_barrier_model(true);
        let dir = unique_dir("ordered");
        std::fs::create_dir_all(&dir).expect("create dir");
        let full_before = thread_full_sync_total();
        let owed_before = owed_generation();
        ordered_write(&dir, "state.bin", Durability::Ordered);
        assert_eq!(thread_sync_count_kind("atomic.file", SyncKind::Barrier), 1);
        assert_eq!(thread_sync_count_kind("dir", SyncKind::Barrier), 1);
        assert_eq!(
            thread_full_sync_total(),
            full_before,
            "no full flush on the op's thread"
        );
        assert!(owed_generation() > owed_before, "a full flush is owed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The flusher thread pays the owed full flush inside the window with nobody calling `flush_now`.
    #[test]
    fn the_flusher_pays_an_owed_full_flush_within_the_window() {
        set_thread_barrier_model(true);
        let dir = unique_dir("ticker");
        std::fs::create_dir_all(&dir).expect("create dir");
        ordered_write(&dir, "state.bin", Durability::Ordered);
        let mine = owed_generation();
        let started = Instant::now();
        while flushed_generation() < mine {
            assert!(
                started.elapsed() < FULL_FLUSH_PERIOD * 3,
                "the owed flush was not paid within the window"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            FULL_FLUSH_PERIOD * 2 <= Duration::from_secs(1),
            "period plus a late landing stays within the 1 s window"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A durability point calls `flush_now`: when it returns, everything ordered before it is covered, with no
    /// waiting on the flusher.
    #[test]
    fn flush_now_covers_every_earlier_ordered_persist_synchronously() {
        set_thread_barrier_model(true);
        let dir = unique_dir("flush-now");
        std::fs::create_dir_all(&dir).expect("create dir");
        ordered_write(&dir, "a.bin", Durability::Ordered);
        ordered_write(&dir, "b.bin", Durability::Ordered);
        let mine = owed_generation();
        flush_now().expect("flush now");
        assert!(
            flushed_generation() >= mine,
            "flush_now returned before covering this thread's persists"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Durability points keep full strength even where barriers exist: the default atomic write and the plain
    /// traced sync are full, never barriers, and they owe nothing.
    #[test]
    fn full_durability_stays_full_where_barriers_exist() {
        set_thread_barrier_model(true);
        let dir = unique_dir("full");
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("key.bin");
        write_bytes_atomic(&path, &atomic_temp_path(&path), b"secret", None).expect("write");
        assert_eq!(thread_sync_count_kind("atomic.file", SyncKind::Full), 1);
        assert_eq!(thread_sync_count_kind("dir", SyncKind::Full), 1);
        assert_eq!(thread_sync_count_kind("atomic.file", SyncKind::Barrier), 0);
        let file = std::fs::File::open(&path).expect("open");
        traced_sync(&file, "key").expect("sync");
        assert_eq!(thread_sync_count_kind("key", SyncKind::Full), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where there are no barriers (Linux, which runs the cells) an ordered persist is the same full sync as
    /// before: nothing is weakened.
    #[test]
    fn without_barriers_an_ordered_persist_is_a_full_sync() {
        set_thread_barrier_model(false);
        let dir = unique_dir("no-barrier");
        std::fs::create_dir_all(&dir).expect("create dir");
        ordered_write(&dir, "state.bin", Durability::Ordered);
        assert_eq!(thread_sync_count_kind("atomic.file", SyncKind::Full), 1);
        assert_eq!(thread_sync_count_kind("dir", SyncKind::Full), 1);
        assert_eq!(thread_sync_count_kind("atomic.file", SyncKind::Barrier), 0);
        let _ = std::fs::remove_dir_all(&dir);
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
