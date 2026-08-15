//! Cross-process file locks via `fslock`, used to serialize installs of the same
//! tool version and to guard store GC.

use std::path::{Path, PathBuf};

use crate::dirs::create_dir_all;
use crate::error::{Error, Result};

/// A held exclusive lock. Released on drop.
pub struct FileLock {
    _inner: fslock::LockFile,
    path: PathBuf,
}

impl FileLock {
    /// Acquire an exclusive lock at `path`, blocking until available. The
    /// parent directory is created if needed.
    pub fn acquire(path: impl AsRef<Path>) -> Result<FileLock> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        let mut lf = fslock::LockFile::open(&path).map_err(|e| Error::io(&path, e))?;
        lf.lock().map_err(|e| Error::io(&path, e))?;
        Ok(FileLock { _inner: lf, path })
    }

    /// Try to acquire without blocking. Returns `Ok(None)` if already held.
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Option<FileLock>> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        let mut lf = fslock::LockFile::open(&path).map_err(|e| Error::io(&path, e))?;
        if lf.try_lock().map_err(|e| Error::io(&path, e))? {
            Ok(Some(FileLock { _inner: lf, path }))
        } else {
            Ok(None)
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
