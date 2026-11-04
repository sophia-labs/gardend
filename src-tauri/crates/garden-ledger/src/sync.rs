//! Durability primitives for the ledger: no statics, no threads, no tracing.
//! The host owns the cadence; this leaf only issues the syscalls.

use std::fs::File;
use std::io;
use std::path::Path;

/// Ordering barrier. On macOS this is `fcntl(F_BARRIERFSYNC)`, falling back to
/// `sync_data` when the filesystem does not support it; elsewhere `sync_data`.
pub fn sync_barrier(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        use std::os::raw::c_int;
        extern "C" {
            fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        }
        const F_BARRIERFSYNC: c_int = 85;
        const EINVAL: i32 = 22;
        const ENOTTY: i32 = 25;
        const ENOTSUP: i32 = 45;
        // SAFETY: fcntl with F_BARRIERFSYNC takes no argument and the fd is
        // valid for the duration of the borrow of `file`.
        let rc = unsafe { fcntl(file.as_raw_fd(), F_BARRIERFSYNC) };
        if rc != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(EINVAL) | Some(ENOTSUP) | Some(ENOTTY) => file.sync_data(),
            _ => Err(err),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        file.sync_data()
    }
}

/// Full flush (`F_FULLFSYNC` on macOS, `fsync(2)` elsewhere), via `File::sync_all`.
pub fn sync_full(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Sync a directory so a newly created file's entry is durable.
/// On non-unix platforms opening a directory may fail; that error is returned.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    let d = File::open(dir)?;
    d.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn syncs_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = File::create(dir.path().join("a")).unwrap();
        f.write_all(b"x").unwrap();
        sync_barrier(&f).unwrap();
        sync_full(&f).unwrap();
        #[cfg(unix)]
        sync_dir(dir.path()).unwrap();
    }
}
