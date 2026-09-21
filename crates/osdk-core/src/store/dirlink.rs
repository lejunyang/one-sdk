//! Directory links: one link primitive, chosen per platform for the one that
//! works without privilege.
//!
//! # Why not `std::os::*::fs::symlink*`
//!
//! On Windows a symlink needs `SeCreateSymbolicLinkPrivilege`, which means
//! elevation or Developer Mode. A junction needs neither. Measured on this
//! project's Windows host: `New-Item -ItemType SymbolicLink` succeeded only
//! because Developer Mode happened to be enabled
//! (`AllowDevelopmentWithoutDevLicense = 1`) while `whoami /priv` listed no
//! `SeCreateSymbolicLinkPrivilege` at all -- so the success was a property of
//! that machine, not of the API, and code relying on it fails on an ordinary
//! user's box. A junction worked with no privilege, could be read through, could
//! be retargeted in place, and deleting it left the target untouched.
//!
//! `store::link::LinkMode` reaches the same conclusion for *files* and says so
//! in its own header ("we never auto-select symlink"). This module is the
//! directory-level counterpart.
//!
//! # Scope
//!
//! Junctions are directory-only and cannot cross machines, which is exactly the
//! use this module serves: a stable local name for a directory whose real
//! location changes. Unlike a hardlink, a junction *can* cross volumes.

use std::path::Path;

/// Create a directory link at `link` resolving to `target`.
///
/// Windows: an NTFS junction. Unix: a symlink. `link` must not already exist;
/// use [`retarget`] to move an existing one.
pub fn create(target: &Path, link: &Path) -> std::io::Result<()> {
    platform::create(target, link)
}

/// Point an existing link at a new target, creating it if absent.
///
/// The link is removed and recreated rather than edited: there is no atomic
/// "repoint" for either primitive, and removing a link never touches what it
/// pointed at.
///
/// Refuses when `link` exists but is a *real* directory. That case means
/// something other than this code owns the path -- user data, or a payload some
/// earlier version wrote in place -- and silently deleting it would destroy it.
pub fn retarget(target: &Path, link: &Path) -> std::io::Result<()> {
    if let Ok(meta) = link.symlink_metadata() {
        if !is_link(&meta) {
            return Err(std::io::Error::other(format!(
                "refusing to replace a real directory with a link: {}",
                link.display()
            )));
        }
        remove(link)?;
    }
    create(target, link)
}

/// Remove a directory link, leaving its target untouched.
///
/// Uses `symlink_metadata`: a dangling junction has no `metadata()` at all, and
/// that is precisely the state this has to be able to clean up.
pub fn remove(link: &Path) -> std::io::Result<()> {
    match link.symlink_metadata() {
        Ok(meta) if is_link(&meta) => platform::remove(link),
        // Nothing there, or not ours to delete. Both are "already not a link".
        _ => Ok(()),
    }
}

/// Whether `meta` describes a directory link this module could have created.
///
/// A Windows junction is a directory carrying `FILE_ATTRIBUTE_REPARSE_POINT`;
/// `is_symlink()` alone does not report it, so a junction would otherwise look
/// like a real directory and never be replaced.
pub fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink() || platform::is_reparse_point(meta)
}

/// Whether `link` is currently a directory link.
pub fn exists(link: &Path) -> bool {
    link.symlink_metadata()
        .map(|meta| is_link(&meta))
        .unwrap_or(false)
}

/// Unlink every directory-link that is a *direct child* of `root`, recursively.
///
/// Used before removing a rendered view tree. A view contains junctions/symlinks
/// that point at snapshot components. `remove_dir_all` on modern Rust does not
/// follow them (the same guarantee relied on for `current`), but Windows
/// junctions sit on undocumented ground, and a view can hold many of them. This
/// walks bottom-up and unlinks each reparse point first, so the subsequent
/// `remove_dir_all` only ever deletes real directories and files inside the
/// view -- never anything a link pointed at.
///
/// File hardlinks are intentionally left alone: removing a hardlink entry only
/// drops one name, never the shared bytes, so ordinary file deletion is correct
/// for them.
pub fn remove_tree_links_first(root: &Path) -> std::io::Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if let Ok(meta) = path.symlink_metadata() {
                    if is_link(&meta) {
                        // A link to a directory: unlink, do not descend into the
                        // target.
                        platform::remove(&path)?;
                        continue;
                    }
                }
                stack.push(path);
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    pub(super) fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    /// Removing a junction unlinks it without touching the target, and a
    /// junction is a directory, so `remove_dir` is the right call.
    pub(super) fn remove(link: &Path) -> std::io::Result<()> {
        std::fs::remove_dir(link)
    }

    /// Create an NTFS directory junction, via the reparse-point ioctl.
    pub(super) fn create(target: &Path, link: &Path) -> std::io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
        use windows_sys::Win32::System::IO::DeviceIoControl;

        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
        const GENERIC_WRITE: u32 = 0x4000_0000;

        // A junction target must be fully qualified in the NT namespace
        // (`\??\C:\...`); a plain path or a `\\?\` prefix is not accepted.
        let absolute = std::fs::canonicalize(target)?;
        let native = absolute.to_string_lossy().replace("\\\\?\\", "");
        let substitute: Vec<u16> = format!("\\??\\{native}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let print: Vec<u16> = native.encode_utf16().chain(std::iter::once(0)).collect();

        // The reparse point is set on an *existing empty directory*.
        std::fs::create_dir(link)?;

        let substitute_bytes = substitute.len() * 2;
        let print_bytes = print.len() * 2;
        let data_len = 8 + substitute_bytes + print_bytes;
        let mut buffer: Vec<u8> = Vec::with_capacity(8 + data_len);
        buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buffer.extend_from_slice(&(data_len as u16).to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes()); // Reserved
        buffer.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
                                                       // Lengths exclude the terminating NUL; offsets are byte offsets into the
                                                       // path buffer.
        buffer.extend_from_slice(&((substitute_bytes - 2) as u16).to_le_bytes());
        buffer.extend_from_slice(&(substitute_bytes as u16).to_le_bytes()); // PrintNameOffset
        buffer.extend_from_slice(&((print_bytes - 2) as u16).to_le_bytes());
        for unit in substitute.iter().chain(print.iter()) {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }

        let wide: Vec<u16> = link
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the handle is
        // closed on every path below.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let error = std::io::Error::last_os_error();
            let _ = std::fs::remove_dir(link);
            return Err(error);
        }
        // SAFETY: `handle` is a valid directory handle opened for write above,
        // and `buffer` describes exactly `buffer.len()` initialised bytes.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let result = if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        };
        // SAFETY: closing a handle we opened and no longer use.
        unsafe {
            CloseHandle(handle);
        }
        if result.is_err() {
            let _ = std::fs::remove_dir(link);
        }
        result
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    pub(super) fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
        false
    }

    /// A Unix symlink to a directory is removed with `remove_file`;
    /// `remove_dir` would fail on it.
    pub(super) fn remove(link: &Path) -> std::io::Result<()> {
        std::fs::remove_file(link)
    }

    pub(super) fn create(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs on every platform: a junction on Windows, a symlink elsewhere. The
    /// behaviour the callers depend on is identical, so the test is not
    /// `#[cfg]`-gated -- gating it would declare half the code unverified on the
    /// other platform, which is where this class of bug hides.
    #[test]
    fn a_link_reads_through_and_can_be_retargeted_without_harming_targets() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("who.txt"), b"first").unwrap();
        std::fs::write(second.join("who.txt"), b"second").unwrap();

        let link = temp.path().join("current");
        create(&first, &link).unwrap();
        assert!(exists(&link));
        assert_eq!(std::fs::read(link.join("who.txt")).unwrap(), b"first");

        // Retarget in place: the stable name keeps working, the content changes.
        retarget(&second, &link).unwrap();
        assert_eq!(std::fs::read(link.join("who.txt")).unwrap(), b"second");

        // Both targets survive being linked and unlinked.
        remove(&link).unwrap();
        assert!(!exists(&link));
        assert_eq!(std::fs::read(first.join("who.txt")).unwrap(), b"first");
        assert_eq!(std::fs::read(second.join("who.txt")).unwrap(), b"second");
    }

    /// `retarget` creates the link when there is nothing there yet, so callers
    /// do not need to special-case the first publish.
    #[test]
    fn retarget_creates_a_missing_link() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("who.txt"), b"target").unwrap();
        let link = temp.path().join("current");

        retarget(&target, &link).unwrap();
        assert_eq!(std::fs::read(link.join("who.txt")).unwrap(), b"target");
    }

    /// A real directory at the link path is not ours to delete.
    ///
    /// Without this, a stray directory -- user data, or a payload an earlier
    /// version wrote in place -- would be silently removed by the next publish.
    #[test]
    fn retarget_refuses_to_delete_a_real_directory() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let occupied = temp.path().join("current");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("precious.txt"), b"do not delete").unwrap();

        let error = retarget(&target, &occupied).unwrap_err();
        assert!(
            error.to_string().contains("refusing to replace"),
            "unexpected error: {error}"
        );
        // The point of the refusal: the data is still there.
        assert_eq!(
            std::fs::read(occupied.join("precious.txt")).unwrap(),
            b"do not delete"
        );
    }

    /// Removing something that is not a link is a no-op, not an error, and must
    /// not delete it.
    #[test]
    fn remove_leaves_a_real_directory_alone() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("keep.txt"), b"keep").unwrap();

        remove(&real).unwrap();
        assert!(real.join("keep.txt").is_file());
    }
}
