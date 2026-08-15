//! Link-mode selection: how a file from the content-addressed store is
//! materialized into an install directory.
//!
//! Priority: an explicit mode is honored (with graceful fallback on error);
//! otherwise `Auto` picks the fastest same-filesystem option (hardlink, then
//! reflink), and falls back to copy across filesystems. We never auto-select
//! symlink: it breaks on Windows without privilege and confuses tools that
//! `realpath` their own executable.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// Same-fs hardlink first, else reflink, else copy.
    #[default]
    Auto,
    /// `std::fs::hard_link` (fastest, zero extra space, same-fs only).
    Hardlink,
    /// Copy-on-write clone via `reflink-copy` (falls back to copy).
    Reflink,
    /// Plain byte copy (always works, uses full space).
    Copy,
    /// Symlink into the store (space-free but fragile; opt-in only).
    Symlink,
}

impl fmt::Display for LinkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LinkMode::Auto => "auto",
            LinkMode::Hardlink => "hardlink",
            LinkMode::Reflink => "reflink",
            LinkMode::Copy => "copy",
            LinkMode::Symlink => "symlink",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for LinkMode {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" => LinkMode::Auto,
            "hardlink" | "hard" => LinkMode::Hardlink,
            "reflink" | "clone" | "cow" => LinkMode::Reflink,
            "copy" => LinkMode::Copy,
            "symlink" | "sym" => LinkMode::Symlink,
            other => return Err(Error::config(format!("unknown link mode `{other}`"))),
        })
    }
}

/// Whether two paths live on the same filesystem (device id compare on Unix).
/// On Windows we compare the volume prefix as a best-effort heuristic.
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    // Compare the nearest existing ancestors so this works before the target
    // file is created.
    let ea = nearest_existing(a);
    let eb = nearest_existing(b);
    match (ea, eb) {
        (Some(pa), Some(pb)) => same_device(&pa, &pb),
        _ => false,
    }
}

fn nearest_existing(p: &Path) -> Option<std::path::PathBuf> {
    let mut cur = Some(p);
    while let Some(c) = cur {
        if c.exists() {
            return Some(c.to_path_buf());
        }
        cur = c.parent();
    }
    None
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev(),
        _ => false,
    }
}

#[cfg(windows)]
fn same_device(a: &Path, b: &Path) -> bool {
    fn volume(p: &Path) -> Option<String> {
        p.components().next().map(|c| {
            c.as_os_str().to_string_lossy().to_ascii_uppercase()
        })
    }
    match (volume(a), volume(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Materialize `src` (a store blob) at `dst` using the requested mode.
///
/// `Auto` resolves to hardlink/reflink/copy based on same-fs detection, with
/// automatic fallback if a chosen primitive is unsupported by the filesystem.
/// Returns the mode actually used.
pub fn materialize(src: &Path, dst: &Path, mode: LinkMode) -> Result<LinkMode> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    // Overwrite any stale target.
    if dst.exists() || dst.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(dst);
    }

    match mode {
        LinkMode::Auto => {
            let same_fs = same_filesystem(src, dst);
            if same_fs {
                if try_hardlink(src, dst).is_ok() {
                    return Ok(LinkMode::Hardlink);
                }
                if try_reflink(src, dst)? {
                    return Ok(LinkMode::Reflink);
                }
            } else if try_reflink(src, dst)? {
                // Some filesystems support reflink across subvolumes.
                return Ok(LinkMode::Reflink);
            }
            copy(src, dst)?;
            Ok(LinkMode::Copy)
        }
        LinkMode::Hardlink => {
            if try_hardlink(src, dst).is_ok() {
                Ok(LinkMode::Hardlink)
            } else {
                // graceful fallback
                copy(src, dst)?;
                Ok(LinkMode::Copy)
            }
        }
        LinkMode::Reflink => {
            if try_reflink(src, dst)? {
                Ok(LinkMode::Reflink)
            } else {
                copy(src, dst)?;
                Ok(LinkMode::Copy)
            }
        }
        LinkMode::Copy => {
            copy(src, dst)?;
            Ok(LinkMode::Copy)
        }
        LinkMode::Symlink => {
            symlink_file(src, dst)?;
            Ok(LinkMode::Symlink)
        }
    }
}

fn try_hardlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::hard_link(src, dst)
}

/// Returns Ok(true) if reflink succeeded, Ok(false) if unsupported (caller
/// should fall back). Propagates only unexpected IO errors.
fn try_reflink(src: &Path, dst: &Path) -> Result<bool> {
    match reflink_copy::reflink(src, dst) {
        Ok(()) => Ok(true),
        Err(e) if is_unsupported(&e) => Ok(false),
        Err(e) => Err(Error::io(dst, e)),
    }
}

fn is_unsupported(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), Unsupported | InvalidInput)
        // cross-device / not-supported errnos surface as Other on some platforms
        || e.raw_os_error()
            .map(|n| n == 18 /* EXDEV */ || n == 95 /* EOPNOTSUPP */ || n == 38 /* ENOSYS */)
            .unwrap_or(false)
}

fn copy(src: &Path, dst: &Path) -> Result<()> {
    std::fs::copy(src, dst).map_err(|e| Error::io(dst, e))?;
    Ok(())
}

#[cfg(unix)]
fn symlink_file(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst).map_err(|e| Error::io(dst, e))
}

#[cfg(windows)]
fn symlink_file(src: &Path, dst: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(src, dst).map_err(|e| Error::io(dst, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(p: &Path, b: &[u8]) {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(b).unwrap();
    }

    #[test]
    fn same_fs_within_tempdir() {
        let td = tempfile::tempdir().unwrap();
        let a = td.path().join("a");
        let b = td.path().join("sub/b");
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        write(&a, b"x");
        // both under same tempdir ⇒ same device
        assert!(same_filesystem(&a, &b));
    }

    #[test]
    fn auto_materialize_hardlinks_same_fs() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("blob");
        write(&src, b"hello");
        let dst = td.path().join("out/file");
        let used = materialize(&src, &dst, LinkMode::Auto).unwrap();
        // same tempdir ⇒ hardlink expected
        assert_eq!(used, LinkMode::Hardlink);
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
    }

    #[test]
    fn copy_mode_duplicates_content() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("blob");
        write(&src, b"data");
        let dst = td.path().join("copy");
        let used = materialize(&src, &dst, LinkMode::Copy).unwrap();
        assert_eq!(used, LinkMode::Copy);
        assert_eq!(std::fs::read(&dst).unwrap(), b"data");
    }

    #[test]
    fn parse_link_mode() {
        assert_eq!("auto".parse::<LinkMode>().unwrap(), LinkMode::Auto);
        assert_eq!("hard".parse::<LinkMode>().unwrap(), LinkMode::Hardlink);
        assert_eq!("cow".parse::<LinkMode>().unwrap(), LinkMode::Reflink);
        assert!("bogus".parse::<LinkMode>().is_err());
    }
}
