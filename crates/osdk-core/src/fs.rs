//! Filesystem primitives shared across the codebase.
//!
//! Before this module the crash-safe "write to a temp file, then replace the
//! target" dance was open-coded in a dozen places, and its two hardest parts --
//! the Windows atomic replace and the parent-directory fsync -- were copy-pasted
//! verbatim (`trust.rs`, `inventory.rs`, `container/apply.rs`, plus the CLI's
//! `lockfile` and `config_edit`). Every copy was a place the platform-specific
//! behavior could silently drift. These two functions are the single source of
//! truth for that pair; callers keep their own error wrapping, temp-file naming
//! and fsync policy on top.

use std::path::Path;

/// Atomically replace `destination` with `source` (a rename that overwrites).
///
/// On Unix this is a plain `rename`, which is atomic within a filesystem. On
/// Windows a plain rename fails when the destination exists, so it goes through
/// `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`:
/// `REPLACE_EXISTING` gives the overwrite Unix already has, and `WRITE_THROUGH`
/// makes the call return only once the change is flushed, matching the intent of
/// the durable writers that use this.
///
/// Errors are returned raw (`std::io::Error`) so each caller can attach its own
/// localized context; that is deliberately not this function's job.
#[cfg(not(windows))]
pub fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

/// See the Unix variant above for the contract; this is the Windows half.
#[cfg(windows)]
pub fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let result = unsafe { MoveFileExW(source_wide.as_ptr(), destination_wide.as_ptr(), flags) };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Flush a directory entry so a just-completed rename survives a crash.
///
/// On Unix a rename is only durable once the *directory* is fsynced, so open it
/// and `sync_all`. On non-Unix this is a no-op: the platforms we target either
/// have no such requirement or fold it into `WRITE_THROUGH` on the replace.
///
/// Errors are returned raw so callers can localize them.
#[cfg(unix)]
pub fn sync_parent(parent: &Path) -> std::io::Result<()> {
    std::fs::File::open(parent).and_then(|directory| directory.sync_all())
}

/// See the Unix variant above for the contract; this is the no-op half.
#[cfg(not(unix))]
pub fn sync_parent(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Non-durable atomic write: create the parent, write a per-process temp file,
/// then [tomic_replace] it onto path. The rename is atomic, so a reader
/// never sees a half-written file, but this deliberately does *not* fsync -- it
/// is the right level for caches and regenerable metadata, not for a lock file
/// that must survive a power cut (those still open-code the durable dance with
/// create_new + sync_all). The temp file uses std::process::id, so two
/// processes writing the same path do not collide.
///
/// Errors are returned raw; callers attach their own context.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    atomic_replace(&temporary, path)
}

/// Remove a directory tree, clearing read-only files that would block it.
///
/// `std::fs::remove_dir_all` fails with `PermissionDenied` on Windows as soon
/// as one entry is read-only, and tools legitimately leave such files behind:
/// a `cargo install` run populates its `HOME`/`CARGO_HOME` with a read-only
/// registry cache, so cleaning the staging workspace failed *after* the
/// install itself had succeeded. Measured on a real stage: the same delete
/// succeeds once the attribute is cleared.
///
/// The first attempt is the plain delete, so the common case costs nothing;
/// only a `PermissionDenied` triggers the walk that clears attributes.
pub fn remove_dir_all_forced(path: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        return match std::fs::remove_dir_all(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    }

    #[cfg(windows)]
    {
        // Windows refuses to unlink a file that is read-only *or* still held open,
        // and both happen here: cargo leaves a read-only registry cache behind, and
        // handles from the child it just ran (plus antivirus scanning the fresh
        // tree) can outlive its exit by a moment. Measured: the identical delete
        // succeeds shortly afterwards. So clear the attributes and give the
        // stragglers a bounded window rather than failing a completed install.
        let mut delay = std::time::Duration::from_millis(20);
        let mut last = None;
        for attempt in 0..8 {
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    last = Some(error);
                    clear_readonly_recursively(path)?;
                    if attempt + 1 < 8 {
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(std::time::Duration::from_millis(500));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "directory could not be removed",
            )
        }))
    }
}

/// Drop the Windows read-only attribute from every file under `path`.
///
/// Errors on individual entries are ignored on purpose: the caller is about to
/// attempt the delete again, and that delete is what reports a real failure.
/// Giving up here because one entry could not be touched would turn a
/// recoverable cleanup into a hard error.
#[cfg(windows)]
fn clear_readonly_recursively(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let _ = clear_readonly_recursively(&entry.path());
            }
        }
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = std::fs::set_permissions(path, permissions);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_replace_overwrites_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"old").unwrap();

        atomic_replace(&source, &destination).unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        assert!(!source.exists(), "source is consumed by the replace");
    }

    #[test]
    fn atomic_replace_creates_destination_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        std::fs::write(&source, b"payload").unwrap();

        atomic_replace(&source, &destination).unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"payload");
    }

    #[test]
    fn forced_removal_deletes_a_tree_containing_read_only_files() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        let nested = tree.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let locked = nested.join("locked.txt");
        std::fs::write(&locked, b"payload").unwrap();
        let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&locked, permissions).unwrap();

        // This is the case a plain remove_dir_all cannot handle on Windows,
        // and it is exactly what a finished `cargo install` leaves behind.
        remove_dir_all_forced(&tree).unwrap();

        assert!(!tree.exists());
    }

    #[test]
    fn forced_removal_is_quiet_about_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        remove_dir_all_forced(&dir.path().join("absent")).unwrap();
    }

    #[test]
    fn sync_parent_accepts_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        // On Unix this fsyncs the directory; elsewhere it is a no-op. Either way
        // a real directory must not error.
        sync_parent(dir.path()).unwrap();
    }
}
