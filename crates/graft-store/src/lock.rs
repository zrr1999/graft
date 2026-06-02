//! Filesystem-backed exclusive lock for `.graft/`.
//!
//! Only one daemon writer may hold the
//! lock at a time. The lock is advisory `flock(LOCK_EX | LOCK_NB)` on a
//! sentinel file at `.graft/.lock`; readers do not need it. The lock is
//! released automatically when the [`WriteLock`] is dropped or the holding
//! process exits, so a crashed graftd cannot leave a permanent lock file.
//!
//! Why advisory flock and not e.g. a PID-bearing lockfile?
//!
//! - flock is released by the kernel on process exit; we don't need crash
//!   recovery code or stale-lock heuristics.
//! - it is non-cooperative across mounts that don't support flock (NFS<v4
//!   without lockd), which we accept as a known limit; graft is intended
//!   for local workspaces.
//! - acquisition is non-blocking (`LOCK_NB`): a contending caller gets
//!   `WouldBlock` immediately, which we surface as a typed error so the
//!   CLI can render `[E_LOCKED]` with a fix hint.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::StoreError;

/// Exclusive write lock on `.graft/.lock`.
///
/// Drop releases the lock. Created via [`WriteLock::acquire`].
#[derive(Debug)]
pub struct WriteLock {
    path: PathBuf,
    file: File,
}

impl WriteLock {
    /// Try to acquire the exclusive lock for the given `.graft/` root.
    ///
    /// `graft_root` is the `.graft/` directory itself, not the workspace.
    /// Caller is responsible for ensuring `graft_root` exists; this is
    /// normally guaranteed because `init_storage()` creates it before any
    /// writer needs the lock.
    ///
    /// Returns [`StoreError::Locked`] when another process already holds
    /// the lock. The returned error carries the lock path so callers can
    /// render an actionable hint.
    pub fn acquire(graft_root: &Path) -> Result<Self, StoreError> {
        let path = graft_root.join(".lock");
        // Make sure the directory exists; lock is meaningless without it.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        let fd = file.as_raw_fd();
        // SAFETY: fd is owned by `file` for the duration of this call.
        let rc = unsafe { libc_flock(fd, LOCK_EX | LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(if err.kind() == std::io::ErrorKind::WouldBlock {
                StoreError::Locked { path }
            } else {
                StoreError::Io(err)
            });
        }
        Ok(Self { path, file })
    }

    /// Path to the underlying `.graft/.lock` sentinel file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let fd = self.file.as_raw_fd();
        // Best-effort unlock; on success the kernel also releases on close.
        unsafe {
            libc_flock(fd, LOCK_UN);
        }
    }
}

// We only need three flock operations and the libc symbol; pulling in the
// `libc` crate just for these would be heavier than the binding itself. The
// constants below match `<sys/file.h>` on every Unix platform we target
// (Linux, macOS, *BSD).

const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;

unsafe extern "C" {
    fn flock(fd: std::os::raw::c_int, op: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[inline]
unsafe fn libc_flock(fd: std::os::raw::c_int, op: i32) -> std::os::raw::c_int {
    unsafe { flock(fd, op) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-lock-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_is_exclusive_within_process() {
        let dir = temp_dir("exclusive");
        let first = WriteLock::acquire(&dir).unwrap();
        let second = WriteLock::acquire(&dir);
        assert!(matches!(second, Err(StoreError::Locked { .. })));
        drop(first);
        // After release a fresh acquire succeeds.
        let _third = WriteLock::acquire(&dir).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn drop_releases_the_lock() {
        let dir = temp_dir("release");
        {
            let _guard = WriteLock::acquire(&dir).unwrap();
        }
        // No contender held it; new acquire works.
        let _later = WriteLock::acquire(&dir).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }
}
