//! `promotion` free functions split from global_npm_use.rs.

use super::*;

pub(crate) fn snapshot_path(path: &Path) -> Result<PathSnapshot> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Ok(PathSnapshot::Symlink(std::fs::read_link(path)?))
        }
        Ok(metadata) if metadata.is_file() => Ok(PathSnapshot::File {
            bytes: std::fs::read(path)?,
            permissions: metadata.permissions(),
        }),
        Ok(_) => Err(anyhow!("cannot snapshot non-file path {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PathSnapshot::Absent),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn restore_path(path: &Path, snapshot: &PathSnapshot) -> Result<()> {
    match snapshot {
        PathSnapshot::Absent => remove_path(path),
        PathSnapshot::File { bytes, permissions } => {
            replace_file_atomically(path, bytes, Some(permissions))
        }
        PathSnapshot::Symlink(target) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temporary = sibling_transaction_path(path, "restore-link");
            remove_path(&temporary)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &temporary)?;
            #[cfg(windows)]
            {
                let target_is_dir = path
                    .parent()
                    .map(|parent| parent.join(target))
                    .unwrap_or_else(|| target.clone())
                    .is_dir();
                if target_is_dir {
                    std::os::windows::fs::symlink_dir(target, &temporary)?;
                } else {
                    std::os::windows::fs::symlink_file(target, &temporary)?;
                }
            }
            replace_path_from_staging(&temporary, path)
        }
    }
}

pub(crate) fn replace_file_atomically(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let temporary = sibling_transaction_path(path, "restore-file");
    std::fs::write(&temporary, bytes)?;
    if let Some(permissions) = permissions {
        std::fs::set_permissions(&temporary, permissions.clone())?;
    }
    replace_path_from_staging(&temporary, path)
}

pub(crate) fn replace_path_from_staging(staging: &Path, final_path: &Path) -> Result<()> {
    let backup = sibling_transaction_path(final_path, "restore-backup");
    let had_previous = std::fs::symlink_metadata(final_path).is_ok();
    if had_previous {
        rename_directory(final_path, &backup)?;
    }
    if let Err(error) = rename_directory(staging, final_path) {
        let restore = had_previous
            .then(|| rename_directory(&backup, final_path).err())
            .flatten()
            .map(anyhow::Error::new);
        remove_path_best_effort(staging);
        return Err(with_rollback_context(
            anyhow::Error::new(error),
            restore,
            None,
        ));
    }
    remove_path_best_effort(&backup);
    Ok(())
}

pub(crate) fn sync_shim_publication(
    dirs: &osdk_core::dirs::Dirs,
    affected_bin_names: &[String],
) -> Result<()> {
    let shims = dirs.shims();
    for name in affected_bin_names {
        sync_file_if_present(&shims.join(name))?;
        #[cfg(windows)]
        sync_file_if_present(&shims.join(format!("{name}.cmd")))?;
    }
    #[cfg(unix)]
    if shims.is_dir() {
        std::fs::File::open(&shims)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing shim directory {}", shims.display()))?;
    }
    Ok(())
}

pub(crate) fn sync_file_if_present(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(()),
        Ok(metadata) if metadata.is_file() => OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("syncing shim {}", path.display())),
        Ok(_) => Err(anyhow!(
            "shim publication path is not a file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting shim {}", path.display())),
    }
}

pub(crate) fn publish_then_activate(
    replacement: Option<&PromotedInstall>,
    publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    publish()?;
    if let Some(replacement) = replacement {
        replacement.mark_activated()?;
    }
    Ok(())
}

pub(crate) fn promotion_journal_path(final_root: &Path) -> PathBuf {
    let parent = final_root.parent().unwrap_or_else(|| Path::new("."));
    let name = final_root
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("install"))
        .to_string_lossy();
    parent.join(format!(".{name}{PROMOTION_JOURNAL_SUFFIX}"))
}

pub(crate) fn promotion_journal_temporary_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("promotion"))
        .to_string_lossy();
    parent.join(format!(
        ".{name}.write-{}-{}",
        std::process::id(),
        NEXT_TRANSACTION_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(crate) fn write_promotion_journal(path: &Path, journal: &PromotionJournal) -> Result<()> {
    let bytes = serde_json::to_vec(journal)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = promotion_journal_temporary_path(path);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = replace_file(&temporary, path) {
        remove_path_best_effort(&temporary);
        return Err(error)
            .with_context(|| format!("writing global npm promotion journal {}", path.display()));
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
pub(crate) fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let existing = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let new = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both pointers address NUL-terminated UTF-16 buffers for the
    // duration of the call.
    let replaced = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            new.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn read_promotion_journal(path: &Path) -> Result<Option<PromotionJournal>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing global npm promotion journal {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("reading global npm promotion journal {}", path.display())),
    }
}

/// Recover the canonical install root after an interrupted directory promotion.
///
/// Directory renames reduce the inconsistent interval, but they are not a
/// power-loss transaction. Before the staged tree reaches its canonical path,
/// recovery rolls back. Once `NewPromoted` is durable, recovery keeps the new
/// tree: publication may already have replaced config or lock, so restoring the
/// old tree could make newly published metadata point at the wrong install. A
/// rerun idempotently repairs any config, lock, or shim work that had not yet
/// completed. `Activated` is only recorded after all of that work succeeds.
pub(crate) fn recover_interrupted_promotion(final_root: &Path) -> Result<()> {
    let journal_path = promotion_journal_path(final_root);
    let Some(journal) = read_promotion_journal(&journal_path)? else {
        let backups = transaction_debris_paths(final_root, "backup")?;
        if !final_root.exists() {
            match backups.as_slice() {
                [backup] => {
                    rename_directory(backup, final_root).with_context(|| {
                        format!(
                            "restoring legacy interrupted global npm backup {}",
                            backup.display()
                        )
                    })?;
                }
                [] => {}
                _ => {
                    return Err(anyhow!(
                        "cannot recover {}: multiple global npm backups exist without a journal",
                        final_root.display()
                    ));
                }
            }
        }
        scavenge_transaction_debris(final_root, &[])?;
        return Ok(());
    };
    validate_journal_path(final_root, &journal.stage_root, "stage")?;
    validate_journal_path(final_root, &journal.backup_root, "backup")?;

    if matches!(
        journal.phase,
        PromotionPhase::NewPromoted | PromotionPhase::Activated
    ) {
        if !final_root.exists() {
            return Err(anyhow!(
                "promoted global npm install is missing canonical install {}",
                final_root.display()
            ));
        }
        remove_path(&journal.backup_root)?;
    } else if journal.had_previous && journal.backup_root.exists() {
        if final_root.exists() {
            let failed_root = sibling_transaction_path(final_root, "failed");
            rename_directory(final_root, &failed_root).with_context(|| {
                format!(
                    "moving interrupted global npm replacement {} aside",
                    final_root.display()
                )
            })?;
            if let Err(error) = rename_directory(&journal.backup_root, final_root) {
                let _ = rename_directory(&failed_root, final_root);
                return Err(error).with_context(|| {
                    format!(
                        "restoring interrupted global npm backup {}",
                        journal.backup_root.display()
                    )
                });
            }
            remove_path(&failed_root)?;
        } else {
            rename_directory(&journal.backup_root, final_root).with_context(|| {
                format!(
                    "restoring interrupted global npm backup {}",
                    journal.backup_root.display()
                )
            })?;
        }
    } else if journal.had_previous && !final_root.exists() {
        return Err(anyhow!(
            "interrupted global npm promotion lost both canonical install {} and backup {}",
            final_root.display(),
            journal.backup_root.display()
        ));
    } else if !journal.had_previous {
        remove_path(final_root)?;
    }

    remove_path(&journal.stage_root)?;
    remove_path(&journal.backup_root)?;
    remove_path(&journal_path)?;
    scavenge_transaction_debris(final_root, &[])?;
    Ok(())
}

pub(crate) fn transaction_debris_paths(final_root: &Path, kind: &str) -> Result<Vec<PathBuf>> {
    let parent = final_root
        .parent()
        .ok_or_else(|| anyhow!("global install root has no parent"))?;
    if !parent.exists() {
        return Ok(Vec::new());
    }
    let name = final_root
        .file_name()
        .ok_or_else(|| anyhow!("global install root has no file name"))?
        .to_string_lossy();
    let prefix = format!(".{name}.osdk-{kind}-");
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

pub(crate) fn validate_journal_path(final_root: &Path, candidate: &Path, kind: &str) -> Result<()> {
    let parent = final_root
        .parent()
        .ok_or_else(|| anyhow!("global install root has no parent"))?;
    let final_name = final_root
        .file_name()
        .ok_or_else(|| anyhow!("global install root has no file name"))?
        .to_string_lossy();
    let expected_prefix = format!(".{final_name}.osdk-{kind}-");
    if candidate.parent() != Some(parent)
        || !candidate
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(&expected_prefix))
    {
        return Err(anyhow!(
            "unsafe {kind} path {} in promotion journal for {}",
            candidate.display(),
            final_root.display()
        ));
    }
    Ok(())
}

pub(crate) fn scavenge_transaction_debris(final_root: &Path, keep: &[&Path]) -> Result<()> {
    let parent = final_root
        .parent()
        .ok_or_else(|| anyhow!("global install root has no parent"))?;
    if !parent.exists() {
        return Ok(());
    }
    let name = final_root
        .file_name()
        .ok_or_else(|| anyhow!("global install root has no file name"))?
        .to_string_lossy();
    let prefixes = [
        format!(".{name}.osdk-stage-"),
        format!(".{name}.osdk-backup-"),
        format!(".{name}.osdk-failed-"),
    ];
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if keep.contains(&path.as_path()) {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if prefixes.iter().any(|prefix| file_name.starts_with(prefix)) {
            remove_path(&path)?;
        }
    }
    Ok(())
}

pub(crate) fn sibling_transaction_path(final_root: &Path, kind: &str) -> PathBuf {
    let parent = final_root.parent().unwrap_or_else(|| Path::new("."));
    let name = final_root
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("install"))
        .to_string_lossy();
    let nonce = NEXT_TRANSACTION_ID.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{name}.osdk-{kind}-{}-{nonce}",
        std::process::id()
    ))
}

pub(crate) fn rename_directory(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..4 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = Some(error);
                if attempt < 3 {
                    std::thread::sleep(Duration::from_millis(20 * (attempt + 1)));
                }
            }
        }
    }
    Err(last.expect("rename was attempted"))
}

pub(crate) fn remove_path_best_effort(path: &Path) {
    let _ = remove_path(path);
}

pub(crate) fn remove_path(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)?;
        }
        Ok(_) => {
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(crate) fn rollback_replacement_error(
    replacement: &mut PromotedInstall,
    error: anyhow::Error,
) -> Result<()> {
    match replacement.rollback() {
        Ok(()) => Err(error),
        Err(rollback) => Err(error.context(format!("install rollback failed: {rollback:#}"))),
    }
}

pub(crate) fn with_rollback_context(
    error: anyhow::Error,
    publication: Option<anyhow::Error>,
    replacement: Option<anyhow::Error>,
) -> anyhow::Error {
    let mut messages = Vec::new();
    if let Some(error) = publication {
        messages.push(format!("publication rollback failed: {error:#}"));
    }
    if let Some(error) = replacement {
        messages.push(format!("install rollback failed: {error:#}"));
    }
    if messages.is_empty() {
        error
    } else {
        error.context(messages.join("; "))
    }
}

pub(crate) fn combine_rollback_errors(
    first: Option<anyhow::Error>,
    second: Option<anyhow::Error>,
) -> Option<anyhow::Error> {
    match (first, second) {
        (Some(first), Some(second)) => {
            Some(first.context(format!("shim reconciliation also failed: {second:#}")))
        }
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

/// Callers hold the version mutation lock before entering this helper. That
/// establishes the same mutation -> global order used by publication while
/// serializing root recovery with global uninstall and uninstall recovery.
pub(crate) fn recover_interrupted_promotion_serialized(
    dirs: &osdk_core::dirs::Dirs,
    final_root: &Path,
) -> Result<()> {
    with_global_npm_state_lock(dirs, || recover_interrupted_promotion(final_root))
}
