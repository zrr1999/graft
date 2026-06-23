//! Filesystem-backed exclusive lock for `.graft/`.
//!
//! Only one daemon writer may hold the
//! lock at a time. The lock is an OS file lock on a sentinel file at
//! `.graft/.lock`; readers do not need it. The lock is released automatically
//! when the [`WriteLock`] is dropped or the holding process exits, so a crashed
//! graftd cannot leave a permanent lock file.
//!
//! Why advisory file locking and not e.g. a PID-bearing lockfile?
//!
//! - OS file locks are released on process exit; we don't need crash recovery
//!   code or stale-lock heuristics.
//! - it is non-cooperative across mounts that don't support file locking
//!   reliably, which we accept as a known limit; graft is intended for local
//!   workspaces.
//! - acquisition is non-blocking (`LOCK_NB`): a contending caller gets
//!   `WouldBlock` immediately, which we surface as a typed error so the
//!   CLI can render `[E_LOCKED]` with a fix hint.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

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

        if let Err(err) = file.try_lock_exclusive() {
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
        // Best-effort unlock; on success the OS also releases on close/process exit.
        let _ = self.file.unlock();
    }
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
