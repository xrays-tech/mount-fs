//! Advisory file locking built on `fs4`.
//!
//! Locks are released automatically when the [`FileLock`] is dropped (the
//! underlying file descriptor closes).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

/// An exclusively locked file.
#[derive(Debug)]
pub struct FileLock {
    file: File,
    path: PathBuf,
}

impl FileLock {
    /// Create `path` if needed and acquire an exclusive lock, blocking until
    /// the lock is available.
    pub fn acquire(path: impl AsRef<Path>) -> io::Result<Self> {
        let (file, path) = open_lock_file(path.as_ref())?;
        file.lock_exclusive()?;
        Ok(FileLock { file, path })
    }

    /// Try to acquire an exclusive lock without blocking.
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when another process (or another
    /// open file description) already holds the lock.
    pub fn try_acquire(path: impl AsRef<Path>) -> io::Result<Self> {
        let (file, path) = open_lock_file(path.as_ref())?;
        if file.try_lock_exclusive()? {
            Ok(FileLock { file, path })
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file lock is already held",
            ))
        }
    }

    /// The locked path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Best effort: closing the descriptor also releases the lock.
        // Use UFCS so this resolves to the `fs4` trait method rather than
        // `File::unlock` (stabilized in Rust 1.89, above our MSRV).
        let _ = FileExt::unlock(&self.file);
    }
}

/// Open (creating parent directories) the lock file.
fn open_lock_file(path: &Path) -> io::Result<(File, PathBuf)> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok((file, path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mount-fs-lock-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn second_try_acquire_fails_then_succeeds_after_drop() {
        let path = temp_path("exclusive");
        let _ = fs::remove_file(&path);

        let first = FileLock::try_acquire(&path).expect("first lock");
        let second = FileLock::try_acquire(&path);
        assert!(second.is_err());
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::WouldBlock);

        drop(first);
        let third = FileLock::try_acquire(&path).expect("lock after drop");
        drop(third);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn acquire_creates_parent_directories() {
        let dir = temp_path("parentdir");
        let path = dir.join("nested").join("lock");
        let lock = FileLock::acquire(&path).expect("acquire with parents");
        assert!(path.exists());
        assert_eq!(lock.path(), path.as_path());
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(lock);
        let _ = fs::remove_dir_all(&dir);
    }
}
