//! Global `npm:<package>` installation for `osdk use --global`.
//!
//! Global means user-selected and shim-visible in osdk. npm, pnpm, and Aube
//! execute their real global-add modes against an osdk-owned prefix. Aube runs
//! in the `osdk-aube` helper process because its global command owns process
//! cwd and process-global settings. No path mutates the caller's project or an
//! ambient Node installation.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use osdk_core::backend::npm_package::{NpmPackageBackend, LOCKED_NPM_NODE_VERSION_OPTION};
use osdk_core::backend::{Backend, InstallCtx};
use osdk_core::npm_tools::{
    self, NpmInstaller, ToolScope, INSTALLER_OPTION, LOCKED_NPM_INSTALLER_OPTION,
    LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION, LOCKED_NPM_NATIVE_LOCK_KIND_OPTION,
    LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION, LOCKED_NPM_SCOPE_OPTION,
};
use osdk_core::package_registry::{self, PackageManager, RegistryPlan, RegistryProbe};
use osdk_core::pipeline::{self, HashAlgo};
use osdk_core::source::select;
use osdk_core::version::{ToolRequest, ToolVersion, VersionSpec};

use crate::app::App;

const NATIVE_CONFIG_DIR: &str = "native-config";
const PROMOTION_JOURNAL_SUFFIX: &str = ".osdk-promotion.json";
static NEXT_TRANSACTION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct GlobalInstallLayout {
    root: PathBuf,
    project: PathBuf,
    bin: PathBuf,
}

impl GlobalInstallLayout {
    fn for_root(
        root: PathBuf,
        installer: NpmInstaller,
        platform: osdk_core::platform::Platform,
    ) -> Self {
        let project = root.join("project");
        let bin =
            if installer == NpmInstaller::Npm && platform.os == osdk_core::platform::Os::Windows {
                root.clone()
            } else {
                root.join("bin")
            };
        Self { root, project, bin }
    }
}

struct StagedInstall {
    final_root: PathBuf,
    stage_root: PathBuf,
    backup_root: PathBuf,
    journal_path: PathBuf,
    promoted: bool,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PromotionPhase {
    Prepared,
    OldMoved,
    NewPromoted,
    Activated,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct PromotionJournal {
    stage_root: PathBuf,
    backup_root: PathBuf,
    had_previous: bool,
    phase: PromotionPhase,
}

impl StagedInstall {
    fn begin(final_root: &Path) -> Result<Self> {
        let parent = final_root.parent().ok_or_else(|| {
            anyhow!(
                "global install root has no parent: {}",
                final_root.display()
            )
        })?;
        std::fs::create_dir_all(parent)?;
        let name = final_root
            .file_name()
            .ok_or_else(|| anyhow!("global install root has no file name"))?
            .to_string_lossy();
        let (stage_root, backup_root) = loop {
            let nonce = NEXT_TRANSACTION_ID.fetch_add(1, Ordering::Relaxed);
            let identity = format!("{}-{nonce}", std::process::id());
            let stage = parent.join(format!(".{name}.osdk-stage-{identity}"));
            let backup = parent.join(format!(".{name}.osdk-backup-{identity}"));
            if backup.exists() {
                continue;
            }
            match std::fs::create_dir(&stage) {
                Ok(()) => break (stage, backup),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("creating staged global install {}", stage.display())
                    })
                }
            }
        };
        let journal_path = promotion_journal_path(final_root);
        Ok(Self {
            final_root: final_root.to_path_buf(),
            stage_root,
            backup_root,
            journal_path,
            promoted: false,
        })
    }

    fn root(&self) -> &Path {
        &self.stage_root
    }

    fn promote(mut self) -> Result<PromotedInstall> {
        self.promote_with(rename_directory)
    }

    fn promote_with(
        &mut self,
        mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<PromotedInstall> {
        let had_previous = self.final_root.exists();
        let mut journal = PromotionJournal {
            stage_root: self.stage_root.clone(),
            backup_root: self.backup_root.clone(),
            had_previous,
            phase: PromotionPhase::Prepared,
        };
        write_promotion_journal(&self.journal_path, &journal)?;
        if had_previous {
            if let Err(error) = rename(&self.final_root, &self.backup_root) {
                remove_path_best_effort(&self.journal_path);
                return Err(error).with_context(|| {
                    format!(
                        "moving current global install {} to backup {}",
                        self.final_root.display(),
                        self.backup_root.display()
                    )
                });
            }
            journal.phase = PromotionPhase::OldMoved;
            if let Err(error) = write_promotion_journal(&self.journal_path, &journal) {
                let restore = rename(&self.backup_root, &self.final_root).err();
                if restore.is_none() {
                    remove_path_best_effort(&self.journal_path);
                }
                return Err(with_rollback_context(
                    error,
                    restore.map(anyhow::Error::new),
                    None,
                ));
            }
        }
        if let Err(error) = rename(&self.stage_root, &self.final_root) {
            let restore = if had_previous {
                match rename(&self.backup_root, &self.final_root) {
                    Ok(()) => {
                        // The old tree is live again; no backup should survive
                        // this failed promotion.
                        remove_path_best_effort(&self.backup_root);
                        None
                    }
                    Err(error) => Some(error),
                }
            } else {
                None
            };
            return Err(with_rollback_context(
                anyhow!(error).context(format!(
                    "promoting staged global install {} to {}",
                    self.stage_root.display(),
                    self.final_root.display()
                )),
                restore.map(anyhow::Error::new),
                None,
            ));
        }
        journal.phase = PromotionPhase::NewPromoted;
        if let Err(error) = write_promotion_journal(&self.journal_path, &journal) {
            let failed_root = sibling_transaction_path(&self.final_root, "failed");
            let move_new = rename(&self.final_root, &failed_root).err();
            let restore = if move_new.is_none() && had_previous {
                rename(&self.backup_root, &self.final_root).err()
            } else {
                None
            };
            if move_new.is_none() && restore.is_none() {
                remove_path_best_effort(&failed_root);
                remove_path_best_effort(&self.journal_path);
            }
            return Err(with_rollback_context(
                error,
                move_new.map(anyhow::Error::new),
                restore.map(anyhow::Error::new),
            ));
        }
        self.promoted = true;
        Ok(PromotedInstall {
            final_root: self.final_root.clone(),
            backup_root: self.backup_root.clone(),
            had_previous,
            stage_root: self.stage_root.clone(),
            journal_path: self.journal_path.clone(),
            completed: false,
        })
    }
}

impl Drop for StagedInstall {
    fn drop(&mut self) {
        if !self.promoted {
            remove_path_best_effort(&self.stage_root);
            if self.backup_root.exists() && !self.final_root.exists() {
                let _ = rename_directory(&self.backup_root, &self.final_root);
            }
            if !self.backup_root.exists() || self.final_root.exists() {
                remove_path_best_effort(&self.journal_path);
            }
        }
    }
}

#[derive(Debug)]
struct PromotedInstall {
    final_root: PathBuf,
    backup_root: PathBuf,
    had_previous: bool,
    stage_root: PathBuf,
    journal_path: PathBuf,
    completed: bool,
}

#[derive(Clone)]
enum PathSnapshot {
    Absent,
    File {
        bytes: Vec<u8>,
        permissions: std::fs::Permissions,
    },
    Symlink(PathBuf),
}

struct PublicationSnapshot {
    paths: Vec<(PathBuf, PathSnapshot)>,
}

impl PublicationSnapshot {
    fn capture(app: &App) -> Result<Self> {
        let paths = vec![
            app.ctx.dirs.user_lock_file(),
            app.ctx.dirs.user_config_file(),
        ];
        let paths = paths
            .into_iter()
            .map(|path| Ok((path.clone(), snapshot_path(&path)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { paths })
    }

    fn restore(&self) -> Result<()> {
        let mut errors = Vec::new();
        for (path, snapshot) in &self.paths {
            if let Err(error) = restore_path(path, snapshot) {
                errors.push(format!("{}: {error}", path.display()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "restoring global publication failed: {}",
                errors.join("; ")
            ))
        }
    }
}

fn snapshot_path(path: &Path) -> Result<PathSnapshot> {
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

fn restore_path(path: &Path, snapshot: &PathSnapshot) -> Result<()> {
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

fn replace_file_atomically(
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

fn replace_path_from_staging(staging: &Path, final_path: &Path) -> Result<()> {
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

fn sync_shim_publication(
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

fn sync_file_if_present(path: &Path) -> Result<()> {
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

impl PromotedInstall {
    fn mark_activated(&self) -> Result<()> {
        write_promotion_journal(
            &self.journal_path,
            &PromotionJournal {
                stage_root: self.stage_root.clone(),
                backup_root: self.backup_root.clone(),
                had_previous: self.had_previous,
                phase: PromotionPhase::Activated,
            },
        )
    }

    fn rollback(&mut self) -> Result<()> {
        if self.completed {
            return Ok(());
        }
        let failed_root = sibling_transaction_path(&self.final_root, "failed");
        rename_directory(&self.final_root, &failed_root).with_context(|| {
            format!(
                "moving failed replacement {} aside to {}",
                self.final_root.display(),
                failed_root.display()
            )
        })?;
        if self.had_previous {
            if let Err(error) = rename_directory(&self.backup_root, &self.final_root) {
                return Err(anyhow!(
                    "restoring global install backup {} to {} failed: {error}; failed replacement remains at {}",
                    self.backup_root.display(),
                    self.final_root.display(),
                    failed_root.display()
                ));
            }
        }
        remove_path_best_effort(&failed_root);
        remove_path_best_effort(&self.stage_root);
        remove_path(&self.journal_path)?;
        self.completed = true;
        Ok(())
    }

    fn finish(&mut self) {
        if self.had_previous {
            if let Err(error) = remove_path(&self.backup_root) {
                tracing::warn!(
                    path = %self.backup_root.display(),
                    error = %error,
                    "failed to remove completed global npm install backup"
                );
            }
        }
        remove_path_best_effort(&self.stage_root);
        if let Err(error) = remove_path(&self.journal_path) {
            tracing::warn!(
                path = %self.journal_path.display(),
                error = %error,
                "failed to remove completed global npm promotion journal"
            );
        }
        self.completed = true;
    }
}

fn publish_then_activate(
    replacement: Option<&PromotedInstall>,
    publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    publish()?;
    if let Some(replacement) = replacement {
        replacement.mark_activated()?;
    }
    Ok(())
}

impl Drop for PromotedInstall {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.rollback();
        }
    }
}

fn promotion_journal_path(final_root: &Path) -> PathBuf {
    let parent = final_root.parent().unwrap_or_else(|| Path::new("."));
    let name = final_root
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("install"))
        .to_string_lossy();
    parent.join(format!(".{name}{PROMOTION_JOURNAL_SUFFIX}"))
}

fn promotion_journal_temporary_path(path: &Path) -> PathBuf {
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

fn write_promotion_journal(path: &Path, journal: &PromotionJournal) -> Result<()> {
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
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
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

fn read_promotion_journal(path: &Path) -> Result<Option<PromotionJournal>> {
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
fn recover_interrupted_promotion(final_root: &Path) -> Result<()> {
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

fn transaction_debris_paths(final_root: &Path, kind: &str) -> Result<Vec<PathBuf>> {
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

fn validate_journal_path(final_root: &Path, candidate: &Path, kind: &str) -> Result<()> {
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

fn scavenge_transaction_debris(final_root: &Path, keep: &[&Path]) -> Result<()> {
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

fn sibling_transaction_path(final_root: &Path, kind: &str) -> PathBuf {
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

fn rename_directory(from: &Path, to: &Path) -> std::io::Result<()> {
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

fn remove_path_best_effort(path: &Path) {
    let _ = remove_path(path);
}

fn remove_path(path: &Path) -> Result<()> {
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

fn rollback_replacement_error(
    replacement: &mut PromotedInstall,
    error: anyhow::Error,
) -> Result<()> {
    match replacement.rollback() {
        Ok(()) => Err(error),
        Err(rollback) => Err(error.context(format!("install rollback failed: {rollback:#}"))),
    }
}

fn with_rollback_context(
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

fn combine_rollback_errors(
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

#[derive(Debug, Clone)]
struct ManagedRuntime {
    node_request: ToolRequest,
    node_version: ToolVersion,
    node_bin: PathBuf,
    manager: Option<(ToolRequest, ToolVersion, PathBuf)>,
}

/// Install, globally select, lock, and expose one dynamic npm tool.
pub async fn install(
    app: &mut App,
    mut request: ToolRequest,
    requested_spec: Option<String>,
) -> Result<()> {
    if !request.backend.starts_with("npm:") {
        return Err(anyhow!(
            "global npm installer requires an npm:<package> request"
        ));
    }
    let requested_installer = npm_tools::installer_from_request_options(&request.options)?;
    let cwd = std::env::current_dir().context("getting current dir for global npm install")?;
    let plan = npm_tools::plan_npm_installer(&cwd, requested_installer, ToolScope::Global)?;
    let backend = NpmPackageBackend::from_id(&request.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", request.backend))?;
    let effective = expand_alias(app, &request)?;
    let mut mutation_lock = None;
    let mut final_layout = None;
    let mut runtime = None;
    let mut version = None;
    let mut staged_install = None;

    // An exact request identifies its canonical root without consulting a
    // registry. Recover that root first, then reuse it when its recorded Node
    // identity is backed by a locally installed runtime. This path must stay
    // free of source probes, downloads, post-install hooks, and helpers.
    if let VersionSpec::Exact(exact) = &effective.spec {
        if let Some(existing_runtime) =
            load_existing_managed_runtime(app, backend.id(), exact, plan.installer)?
        {
            set_runtime_request_options(&mut request, plan.installer, &existing_runtime);
            let mut candidate = ToolVersion::new(backend.id(), exact.clone());
            candidate.options = request.options.clone();
            candidate
                .options
                .insert(INSTALLER_OPTION.into(), plan.installer.as_str().into());
            let candidate_layout = GlobalInstallLayout::for_root(
                backend.global_install_root_for(&app.ctx, &candidate)?,
                plan.installer,
                app.ctx.platform,
            );
            let lock = acquire_global_version_lock(app, &backend, &candidate)?;
            recover_interrupted_promotion_serialized(&app.ctx.dirs, &candidate_layout.root)?;
            let installed_native_lock = native_lock_path(&candidate_layout, plan.installer);
            if completed_install_matches_at(
                app.ctx.platform,
                &backend,
                &candidate,
                plan.installer,
                &existing_runtime.node_version.version,
                &candidate_layout,
                installed_native_lock.as_deref(),
            )? {
                let native = installed_native_lock
                    .as_deref()
                    .filter(|path| path.is_file())
                    .map(|path| read_native_lock(path, plan.installer))
                    .transpose()?;
                inject_native_metadata(
                    &mut candidate,
                    plan.installer,
                    &existing_runtime.node_version.version,
                    native.as_ref(),
                );
                runtime = Some(existing_runtime);
                version = Some(candidate);
            }
            mutation_lock = Some(lock);
            final_layout = Some(candidate_layout);
        }
    }

    if version.is_none() {
        // Aube 2.1 global add always resolves online and exposes no offline
        // switch. Exact local recovery/reuse is allowed above, but a new
        // helper install must fail before registry or runtime preparation.
        if plan.installer == NpmInstaller::Aube && app.ctx.config.settings.offline {
            return Err(anyhow!(osdk_core::t!(
                "err.npm_global_aube_offline_unsupported"
            )));
        }
        let package_spec = format!("{}@{}", backend.package(), request.spec);
        let (registry_manager, registry_alias, registry_command) = match plan.installer {
            NpmInstaller::Aube | NpmInstaller::Npm => (PackageManager::Npm, "npm", "install"),
            NpmInstaller::Pnpm => (PackageManager::Pnpm, "pnpm", "add"),
            NpmInstaller::Auto => {
                unreachable!("global planning always produces a concrete installer")
            }
        };
        // This must precede runtime preparation and npm package resolution. A
        // private/authenticated native registry cannot be reproduced inside
        // the isolated global installer.
        let selected_registry = plan_isolated_global_registry(
            app,
            &app.ctx.dirs.data.join("global-registry-preflight"),
            registry_manager,
            registry_alias,
            &[registry_command.into(), package_spec],
        )
        .await?;
        apply_source_override(app, &request.backend);
        let prepared_runtime = ensure_managed_runtime(app, plan.installer).await?;
        set_runtime_request_options(&mut request, plan.installer, &prepared_runtime);

        if app.refresh_sources {
            select::refresh(&app.ctx, &backend).await?;
        }
        let effective = expand_alias(app, &request)?;
        let mut resolved = backend
            .resolve_version(&app.ctx, &effective)
            .await
            .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
        resolved.options = request.options.clone();
        resolved
            .options
            .insert(INSTALLER_OPTION.into(), plan.installer.as_str().into());
        let resolved_layout = GlobalInstallLayout::for_root(
            backend.global_install_root_for(&app.ctx, &resolved)?,
            plan.installer,
            app.ctx.platform,
        );
        if let Some(existing_layout) = &final_layout {
            if existing_layout.root != resolved_layout.root {
                return Err(anyhow!(
                    "exact global npm request resolved to an unexpected version: {}",
                    resolved.version
                ));
            }
        } else {
            mutation_lock = Some(acquire_global_version_lock(app, &backend, &resolved)?);
            recover_interrupted_promotion_serialized(&app.ctx.dirs, &resolved_layout.root)?;
            final_layout = Some(resolved_layout.clone());
        }

        let installed_native_lock = native_lock_path(&resolved_layout, plan.installer);
        if !completed_install_matches_at(
            app.ctx.platform,
            &backend,
            &resolved,
            plan.installer,
            &prepared_runtime.node_version.version,
            &resolved_layout,
            installed_native_lock.as_deref(),
        )? {
            let staged = StagedInstall::begin(&resolved_layout.root)?;
            let staged_layout = GlobalInstallLayout::for_root(
                staged.root().to_path_buf(),
                plan.installer,
                app.ctx.platform,
            );
            run_global_install(
                app,
                &backend,
                &resolved,
                plan.installer,
                &prepared_runtime,
                &staged_layout,
                selected_registry.as_deref(),
            )
            .await?;
            normalize_global_bins(&backend, &resolved, plan.installer, &staged_layout)?;
            validate_global_package_identity(&backend, &resolved, plan.installer, &staged_layout)?;
            let staged_native_lock = native_lock_path(&staged_layout, plan.installer);
            let native = staged_native_lock
                .as_deref()
                .filter(|path| path.is_file())
                .map(|path| read_native_lock(path, plan.installer))
                .transpose()?;
            inject_native_metadata(
                &mut resolved,
                plan.installer,
                &prepared_runtime.node_version.version,
                native.as_ref(),
            );
            backend
                .finalize_global_install_at(
                    &app.ctx,
                    &resolved,
                    &staged_layout.root,
                    &staged_layout.bin,
                    &prepared_runtime.node_version.version,
                    plan.installer.as_str(),
                    native
                        .as_ref()
                        .map(|native| (native.format.as_str(), native.sha256.as_str())),
                )
                .map_err(anyhow::Error::new)?;
            if !completed_install_matches_at(
                app.ctx.platform,
                &backend,
                &resolved,
                plan.installer,
                &prepared_runtime.node_version.version,
                &staged_layout,
                staged_native_lock.as_deref(),
            )? {
                return Err(anyhow!(
                    "staged global npm install failed completed-state validation at {}",
                    staged_layout.root.display()
                ));
            }
            staged_install = Some(staged);
        } else {
            let native = installed_native_lock
                .as_deref()
                .filter(|path| path.is_file())
                .map(|path| read_native_lock(path, plan.installer))
                .transpose()?;
            inject_native_metadata(
                &mut resolved,
                plan.installer,
                &prepared_runtime.node_version.version,
                native.as_ref(),
            );
        }
        runtime = Some(prepared_runtime);
        version = Some(resolved);
    }

    let runtime = runtime.expect("global npm runtime is available after preparation or reuse");
    let version = version.expect("global npm version is available after preparation or reuse");
    let final_layout =
        final_layout.expect("global npm layout is available after preparation or reuse");
    let _mutation_lock =
        mutation_lock.expect("global npm mutation lock is held through publication");

    let persisted_spec = requested_spec.unwrap_or_else(|| version.version.clone());
    with_global_npm_state_lock(&app.ctx.dirs, || {
        crate::commands::recover_interrupted_global_npm_uninstalls(app)?;
        let new_bin_names = manifest_bin_names(
            staged_install
                .as_ref()
                .map(StagedInstall::root)
                .unwrap_or(&final_layout.root),
        )?;
        let old_bin_names = selected_global_bin_names(&app.ctx, &backend, &final_layout.root)?;
        let publication = PublicationSnapshot::capture(app)?;
        let mut replacement = if let Some(staged) = staged_install {
            let mut promoted = staged.promote()?;
            let promoted_native_lock = native_lock_path(&final_layout, plan.installer);
            if !completed_install_matches_at(
                app.ctx.platform,
                &backend,
                &version,
                plan.installer,
                &runtime.node_version.version,
                &final_layout,
                promoted_native_lock.as_deref(),
            )? {
                return rollback_replacement_error(
                    &mut promoted,
                    anyhow!(
                        "promoted global npm install failed validation at {}",
                        final_layout.root.display()
                    ),
                );
            }
            Some(promoted)
        } else {
            None
        };
        // `NewPromoted` is the roll-forward boundary: an abrupt exit keeps the
        // fully validated new root and a rerun repairs any remaining metadata
        // or shims. Do not mark the publication activated until config, lock,
        // and every affected shim directory entry have been durably published.
        // A normal error still rolls every publication step back below.
        let publish_result = publish_then_activate(replacement.as_ref(), || {
            crate::commands::generate_shims_for(app, &backend, &version)?;
            persist_global_config(app, &request, &persisted_spec, plan.installer)?;
            persist_global_lock(app, &request, &version, &runtime)?;
            let refreshed = refreshed_app(app)?;
            crate::commands::remove_stale_shims_without_other_owners(
                &refreshed,
                backend.id(),
                &old_bin_names,
                &new_bin_names,
            )?;
            // This full pass makes the roll-forward path idempotent even when
            // a prior process crashed after publishing config/lock but before
            // it could remove a bin name from the previously selected version.
            crate::commands::reconcile_managed_shims(&refreshed)?;
            let mut affected_bin_names = old_bin_names.clone();
            affected_bin_names.extend(new_bin_names.iter().cloned());
            affected_bin_names.sort();
            affected_bin_names.dedup();
            sync_shim_publication(&app.ctx.dirs, &affected_bin_names)?;
            Ok(())
        });
        if let Err(error) = publish_result {
            let publication_error = publication.restore().err();
            let replacement_error = replacement
                .as_mut()
                .map(PromotedInstall::rollback)
                .transpose()
                .err();
            let shim_error = refreshed_app(app)
                .and_then(|refreshed| crate::commands::reconcile_managed_shims(&refreshed))
                .err();
            return Err(with_rollback_context(
                error,
                combine_rollback_errors(publication_error, shim_error),
                replacement_error,
            ));
        }
        if let Some(mut replacement) = replacement {
            replacement.finish();
        }
        if let Err(error) = backend.remove_legacy_global_install(&app.ctx, &version) {
            tracing::warn!(error = %error, "failed to remove legacy global npm install after publication");
        }
        Ok(())
    })?;
    println!(
        "{}",
        osdk_core::t!(
            "msg.pinned_global",
            tool = request.backend,
            ver = persisted_spec
        )
    );

    Ok(())
}

pub(crate) fn with_global_npm_state_lock<T>(
    dirs: &osdk_core::dirs::Dirs,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _lock = osdk_core::lock::FileLock::acquire(dirs.data.join("locks/global-npm-state.lock"))?;
    operation()
}

fn acquire_global_version_lock(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
) -> Result<osdk_core::lock::FileLock> {
    let locator = backend.install_locator(&app.ctx, version, ToolScope::Global)?;
    osdk_core::lock::FileLock::acquire(locator.lock_path()).map_err(anyhow::Error::new)
}

/// Callers hold the version mutation lock before entering this helper. That
/// establishes the same mutation -> global order used by publication while
/// serializing root recovery with global uninstall and uninstall recovery.
fn recover_interrupted_promotion_serialized(
    dirs: &osdk_core::dirs::Dirs,
    final_root: &Path,
) -> Result<()> {
    with_global_npm_state_lock(dirs, || recover_interrupted_promotion(final_root))
}

fn refreshed_app(app: &App) -> Result<App> {
    let cwd = std::env::current_dir()?;
    let ctx = osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config: osdk_core::config::Config::load(&app.ctx.dirs.user_config_file(), &cwd)?,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: app.ctx.show_progress,
    };
    Ok(App {
        ctx,
        registry: osdk_core::Registry::load(&app.ctx.dirs)?,
        prompt: app.prompt.clone(),
        source_override: app.source_override.clone(),
        refresh_sources: app.refresh_sources,
    })
}

async fn ensure_managed_runtime(app: &mut App, installer: NpmInstaller) -> Result<ManagedRuntime> {
    let node_request = configured_request(app, "node");
    let (node_backend, node_version) = install_backend(app, &node_request).await?;
    let node_bin = find_bin_dir(&app.ctx, node_backend.as_ref(), &node_version, "node")?;
    let manager_id = match installer {
        NpmInstaller::Npm => Some("npm"),
        NpmInstaller::Pnpm => Some("pnpm"),
        NpmInstaller::Aube => None,
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    };
    let manager = if let Some(manager_id) = manager_id {
        let request = configured_request(app, manager_id);
        let (backend, version) = install_backend(app, &request).await?;
        let executable = find_executable(&backend.bin_paths(&app.ctx, &version)?, manager_id)
            .ok_or_else(|| anyhow!("managed {manager_id} executable was not installed"))?;
        Some((exact_request(request, &version), version, executable))
    } else {
        None
    };
    Ok(ManagedRuntime {
        node_request: exact_request(node_request, &node_version),
        node_version,
        node_bin,
        manager,
    })
}

fn set_runtime_request_options(
    request: &mut ToolRequest,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
) {
    request.options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    request.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    request.options.insert(
        LOCKED_NPM_NODE_VERSION_OPTION.into(),
        runtime.node_version.version.clone(),
    );
}

/// Reconstruct a runtime strictly from authoritative local state. The npm
/// package manifest is deliberately not used to choose Node: it is only
/// compared against this independently selected installed runtime later.
fn load_existing_managed_runtime(
    app: &App,
    package_backend: &str,
    package_version: &str,
    installer: NpmInstaller,
) -> Result<Option<ManagedRuntime>> {
    let locked = existing_global_lock_requests(app, package_backend, package_version)?;
    let node_request = locked
        .as_ref()
        .and_then(|requests| exact_locked_request(requests, "node"))
        .cloned()
        .or_else(|| exact_configured_request(app, "node"));
    let Some(node_request) = node_request else {
        return Ok(None);
    };
    let Some((node_version, node_bin)) = load_exact_runtime_component(app, &node_request, "node")?
    else {
        return Ok(None);
    };

    let manager_id = match installer {
        NpmInstaller::Npm => Some("npm"),
        NpmInstaller::Pnpm => Some("pnpm"),
        NpmInstaller::Aube => None,
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    };
    let manager = if let Some(manager_id) = manager_id {
        let manager_request = locked
            .as_ref()
            .and_then(|requests| exact_locked_request(requests, manager_id))
            .cloned()
            .or_else(|| exact_configured_request(app, manager_id));
        let Some(manager_request) = manager_request else {
            return Ok(None);
        };
        let Some((manager_version, executable)) =
            load_exact_runtime_component(app, &manager_request, manager_id)?
        else {
            return Ok(None);
        };
        Some((manager_request, manager_version, executable))
    } else {
        None
    };

    Ok(Some(ManagedRuntime {
        node_request,
        node_version,
        node_bin,
        manager,
    }))
}

fn existing_global_lock_requests(
    app: &App,
    package_backend: &str,
    package_version: &str,
) -> Result<Option<Vec<ToolRequest>>> {
    let path = app.ctx.dirs.user_lock_file();
    if !path.is_file() {
        return Ok(None);
    }
    let Some(requests) = crate::lockfile::locked_requests(&path, app.ctx.platform)? else {
        return Ok(None);
    };
    let matches_package = requests.iter().any(|request| {
        request.backend == package_backend
            && matches!(&request.spec, VersionSpec::Exact(version) if version == package_version)
            && request
                .options
                .get(LOCKED_NPM_SCOPE_OPTION)
                .map(String::as_str)
                == Some(ToolScope::Global.as_str())
    });
    Ok(matches_package.then_some(requests))
}

fn exact_locked_request<'a>(requests: &'a [ToolRequest], backend: &str) -> Option<&'a ToolRequest> {
    requests
        .iter()
        .find(|request| request.backend == backend && matches!(request.spec, VersionSpec::Exact(_)))
}

fn exact_configured_request(app: &App, backend: &str) -> Option<ToolRequest> {
    let request = configured_request(app, backend);
    matches!(request.spec, VersionSpec::Exact(_)).then_some(request)
}

fn load_exact_runtime_component(
    app: &App,
    request: &ToolRequest,
    executable: &str,
) -> Result<Option<(ToolVersion, PathBuf)>> {
    let VersionSpec::Exact(version) = &request.spec else {
        return Ok(None);
    };
    let backend = app.registry.get(&request.backend)?;
    let installed = ToolVersion::new(backend.id(), version.clone());
    if !backend
        .list_installed(&app.ctx)?
        .iter()
        .any(|candidate| candidate == version)
    {
        return Ok(None);
    }
    let Some(path) = find_executable(&backend.bin_paths(&app.ctx, &installed)?, executable) else {
        return Ok(None);
    };
    let location = if executable == "node" {
        path.parent()
            .ok_or_else(|| anyhow!("managed node executable has no parent directory"))?
            .to_path_buf()
    } else {
        path
    };
    Ok(Some((installed, location)))
}

fn exact_request(mut request: ToolRequest, version: &ToolVersion) -> ToolRequest {
    request.spec = VersionSpec::Exact(version.version.clone());
    request
}

fn configured_request(app: &App, backend: &str) -> ToolRequest {
    let entry = app.ctx.config.global_tool_configs.get(backend);
    ToolRequest {
        backend: backend.into(),
        spec: entry
            .map(|entry| VersionSpec::parse(entry.version()))
            .unwrap_or(VersionSpec::Latest),
        options: entry
            .map(|entry| entry.to_request_options())
            .unwrap_or_default(),
    }
}

async fn install_backend(
    app: &mut App,
    request: &ToolRequest,
) -> Result<(std::sync::Arc<dyn Backend>, ToolVersion)> {
    apply_source_override(app, &request.backend);
    let backend = app.registry.get(&request.backend)?;
    if app.refresh_sources {
        select::refresh(&app.ctx, backend.as_ref()).await?;
    }
    let effective = expand_alias(app, request)?;
    let version = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
    if pipeline::is_installed(&app.ctx.dirs, backend.id(), &version.version) {
        backend.ensure_post_install(&app.ctx, &version)?;
    } else {
        backend
            .install(&InstallCtx { ctx: &app.ctx }, &version)
            .await
            .with_context(|| format!("installing {}", version))?;
    }
    Ok((backend, version))
}

async fn run_global_install(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
    layout: &GlobalInstallLayout,
    selected_registry: Option<&str>,
) -> Result<()> {
    match installer {
        NpmInstaller::Aube => {
            run_aube_global_installer(app, backend, version, runtime, layout, selected_registry)
                .await
        }
        NpmInstaller::Npm | NpmInstaller::Pnpm => {
            run_native_installer(
                app,
                backend,
                version,
                installer,
                runtime,
                layout,
                selected_registry,
            )
            .await
        }
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    }
}

async fn run_aube_global_installer(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    runtime: &ManagedRuntime,
    layout: &GlobalInstallLayout,
    selected_registry: Option<&str>,
) -> Result<()> {
    let helper = find_aube_helper()?;
    let package_spec = format!("{}@{}", backend.package(), version.version);
    let native = layout.root.join(NATIVE_CONFIG_DIR);
    let aube_home = native.join("home");
    let aube_global_parent = layout.root.join("aube-global");
    let aube_bin = layout.bin.clone();
    let aube_cache = NpmPackageBackend::aube_cache_dir(&app.ctx);
    let aube_store = NpmPackageBackend::aube_store_dir(&app.ctx);
    let aube_runtime = layout.root.join("aube-runtime-disabled");
    let user_config = native.join("aube.npmrc");
    let global_config = native.join("aube-global.npmrc");
    let xdg_config = native.join("xdg-config");
    let xdg_data = native.join("xdg-data");
    let xdg_cache = native.join("xdg-cache");
    for path in [
        &aube_home,
        &aube_global_parent,
        &aube_bin,
        &aube_cache,
        &aube_store,
        &aube_runtime,
        &native,
        &xdg_config,
        &xdg_data,
        &xdg_cache,
    ] {
        std::fs::create_dir_all(path)?;
    }
    for path in [
        &native,
        &aube_home,
        &aube_global_parent,
        &aube_bin,
        &aube_runtime,
        &xdg_config,
        &xdg_data,
        &xdg_cache,
    ] {
        validate_owned_stage_directory(&layout.root, path)?;
    }
    for path in [&user_config, &global_config] {
        if !path.exists() {
            std::fs::write(path, b"")?;
        }
    }

    let args = aube_global_args(version, package_spec, selected_registry.map(str::to_owned))?;

    let path = std::env::join_paths(std::iter::once(runtime.node_bin.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    let mut env = BTreeMap::from([
        ("PATH".into(), path.to_string_lossy().into_owned()),
        ("HOME".into(), aube_home.display().to_string()),
        ("XDG_CONFIG_HOME".into(), xdg_config.display().to_string()),
        ("XDG_DATA_HOME".into(), xdg_data.display().to_string()),
        ("XDG_CACHE_HOME".into(), xdg_cache.display().to_string()),
        (
            "NPM_CONFIG_USERCONFIG".into(),
            user_config.display().to_string(),
        ),
        (
            "NPM_CONFIG_GLOBALCONFIG".into(),
            global_config.display().to_string(),
        ),
        (
            "NPM_CONFIG_GLOBAL_DIR".into(),
            aube_global_parent.display().to_string(),
        ),
        (
            "NPM_CONFIG_GLOBAL_BIN_DIR".into(),
            aube_bin.display().to_string(),
        ),
        (
            "NPM_CONFIG_STORE_DIR".into(),
            aube_store.display().to_string(),
        ),
        (
            "NPM_CONFIG_CACHE_DIR".into(),
            aube_cache.display().to_string(),
        ),
        (
            "NPM_CONFIG_NODE_VERSION".into(),
            runtime.node_version.version.clone(),
        ),
        (
            "AUBE_RUNTIME_DIR".into(),
            aube_runtime.display().to_string(),
        ),
        ("AUBE_NO_UPDATE_CHECK".into(), "1".into()),
    ]);
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SystemRoot",
        "WINDIR",
        "ComSpec",
        "PATHEXT",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    run_managed_command(&helper, &args, &env, &layout.root)
        .with_context(|| format!("running bundled Aube helper {}", helper.display()))?;

    let package_root = aube_global_parent.join("global-aube");
    let resolved = resolve_aube_global_install(
        &layout.root,
        &package_root,
        backend.package(),
        &version.version,
    )?;
    let destination = layout.project.clone();
    if destination.exists() {
        remove_path(&destination)?;
    }
    // Narrow the pointer-swap race by checking it again immediately before
    // unlinking the Aube-owned stable pointer and moving its physical tree.
    let current_target = dunce::canonicalize(&resolved.pointer).with_context(|| {
        format!(
            "re-resolving Aube global pointer {}",
            resolved.pointer.display()
        )
    })?;
    if current_target != resolved.install_dir {
        return Err(anyhow!(
            "Aube global pointer {} changed during validation",
            resolved.pointer.display()
        ));
    }
    remove_aube_hash_pointer(&resolved.pointer)?;
    std::fs::rename(&resolved.install_dir, &destination).with_context(|| {
        format!(
            "moving Aube global install {} to {}",
            resolved.install_dir.display(),
            destination.display()
        )
    })?;
    validate_aube_project(&destination, backend.package(), &version.version)?;
    remove_path_best_effort(&aube_global_parent);
    remove_path_best_effort(&native);
    remove_path_best_effort(&aube_runtime);
    Ok(())
}

fn find_aube_helper() -> Result<PathBuf> {
    if let Some(override_path) = std::env::var_os("OSDK_AUBE_BIN") {
        if override_path.is_empty() {
            return Err(anyhow!("OSDK_AUBE_BIN must not be empty"));
        }
        let override_path = PathBuf::from(override_path);
        if override_path.is_file() {
            return Ok(override_path);
        }
        return Err(anyhow!(
            "Aube helper override is not a regular file: {}",
            override_path.display()
        ));
    }
    let current = std::env::current_exe().context("locating osdk executable")?;
    let name = if cfg!(windows) {
        "osdk-aube.exe"
    } else {
        "osdk-aube"
    };
    let sibling = current
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name);
    if sibling.is_file() {
        Ok(sibling)
    } else {
        Err(anyhow!(
            "required Aube helper is missing at {}; reinstall osdk with osdk-aube",
            sibling.display()
        ))
    }
}

fn aube_global_args(
    version: &ToolVersion,
    package_spec: String,
    source: Option<String>,
) -> Result<Vec<String>> {
    let mut args = vec![
        "add".to_string(),
        "--global".to_string(),
        "--save-exact".to_string(),
        "--disable-gvs".to_string(),
        "--config.nodeLinker=hoisted".to_string(),
    ];
    match npm_build_policy(version)? {
        NpmBuildPolicy::Deny => {
            // Aube 2.1's global wrapper drops --ignore-scripts when it builds
            // the inner add request. A wildcard deny is the fail-closed
            // equivalent and overrides the built-in trusted dependency list.
            args.push("--deny-build=*".into());
        }
        NpmBuildPolicy::AllowAll => {
            args.push("--dangerously-allow-all-builds".into());
        }
        NpmBuildPolicy::Packages(packages) => {
            for package in packages {
                args.push(format!("--allow-build={package}"));
            }
        }
    }
    if let Some(source) = source {
        args.push(format!("--registry={source}"));
    }
    args.push(package_spec);
    Ok(args)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NpmBuildPolicy {
    Deny,
    AllowAll,
    Packages(Vec<String>),
}

impl NpmBuildPolicy {
    fn identity(&self) -> String {
        match self {
            Self::Deny => "deny".into(),
            Self::AllowAll => "allow-all".into(),
            Self::Packages(packages) => format!("packages:{}", packages.join(",")),
        }
    }
}

fn parse_npm_build_policy(raw: Option<&str>) -> Result<NpmBuildPolicy> {
    let Some(raw) = raw else {
        return Ok(NpmBuildPolicy::Deny);
    };
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    if lower.is_empty() || matches!(lower.as_str(), "false" | "0" | "no" | "off") {
        return Ok(NpmBuildPolicy::Deny);
    }
    if matches!(lower.as_str(), "true" | "1" | "yes" | "on") {
        return Ok(NpmBuildPolicy::AllowAll);
    }
    let mut packages = raw
        .split(',')
        .map(str::trim)
        .filter(|package| !package.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    packages.sort();
    packages.dedup();
    if packages.is_empty() {
        return Err(anyhow!("allow_builds contains no package names"));
    }
    Ok(NpmBuildPolicy::Packages(packages))
}

fn npm_build_policy(version: &ToolVersion) -> Result<NpmBuildPolicy> {
    parse_npm_build_policy(version.options.get("allow_builds").map(String::as_str))
}

fn npm_build_policy_identity(version: &ToolVersion) -> Result<String> {
    Ok(npm_build_policy(version)?.identity())
}

#[derive(Debug)]
struct ResolvedAubeGlobalInstall {
    pointer: PathBuf,
    install_dir: PathBuf,
}

fn resolve_aube_global_install(
    stage_root: &Path,
    package_root: &Path,
    package: &str,
    version: &str,
) -> Result<ResolvedAubeGlobalInstall> {
    let canonical_stage = dunce::canonicalize(stage_root)
        .with_context(|| format!("canonicalizing Aube stage {}", stage_root.display()))?;
    let root_metadata = std::fs::symlink_metadata(package_root)
        .with_context(|| format!("reading Aube global root {}", package_root.display()))?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "Aube global package root is not a real directory: {}",
            package_root.display()
        ));
    }
    let canonical_package_root = dunce::canonicalize(package_root)
        .with_context(|| format!("canonicalizing Aube global root {}", package_root.display()))?;
    if canonical_package_root == canonical_stage
        || !canonical_package_root.starts_with(&canonical_stage)
    {
        return Err(anyhow!(
            "Aube global package root escapes the osdk staging directory: {}",
            canonical_package_root.display()
        ));
    }

    let mut pointer_count = 0usize;
    let mut valid = Vec::new();
    let mut rejected = Vec::new();
    for entry in std::fs::read_dir(&canonical_package_root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_aube_hash_pointer_name(&name) {
            continue;
        }
        pointer_count += 1;
        let pointer = entry.path();
        let result = (|| -> Result<PathBuf> {
            let metadata = std::fs::symlink_metadata(&pointer)?;
            if !is_aube_hash_pointer(&metadata) {
                return Err(anyhow!("hash entry is not a symlink"));
            }
            let target = dunce::canonicalize(&pointer)?;
            let target_metadata = std::fs::symlink_metadata(&target)?;
            if !target_metadata.is_dir() || target_metadata.file_type().is_symlink() {
                return Err(anyhow!("pointer target is not a real directory"));
            }
            if target == canonical_package_root
                || target.parent() != Some(canonical_package_root.as_path())
            {
                return Err(anyhow!("pointer target escapes the Aube global root"));
            }
            validate_aube_project(&target, package, version)?;
            Ok(target)
        })();
        match result {
            Ok(install_dir) => valid.push(ResolvedAubeGlobalInstall {
                pointer,
                install_dir,
            }),
            Err(error) => rejected.push(format!("{name}: {error:#}")),
        }
    }

    match valid.len() {
        1 if pointer_count == 1 => Ok(valid.pop().expect("one candidate exists")),
        0 => Err(anyhow!(
            "Aube global install did not produce exactly one valid hash pointer under {} (found {pointer_count}); {}",
            canonical_package_root.display(),
            if rejected.is_empty() {
                "no 64-character lowercase hash pointers found".to_string()
            } else {
                rejected.join("; ")
            }
        )),
        _ => Err(anyhow!(
            "Aube global install is ambiguous under {}: found {} valid hash pointers ({} total)",
            canonical_package_root.display(),
            valid.len(),
            pointer_count
        )),
    }
}

#[cfg(not(windows))]
fn is_aube_hash_pointer(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_aube_hash_pointer(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn is_aube_hash_pointer_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn remove_aube_hash_pointer(pointer: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        std::fs::remove_dir(pointer)
            .with_context(|| format!("removing Aube global junction {}", pointer.display()))?;
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_file(pointer)
            .with_context(|| format!("removing Aube global symlink {}", pointer.display()))?;
    }
    Ok(())
}

fn validate_aube_project(project: &Path, package: &str, version: &str) -> Result<()> {
    let project_metadata = std::fs::symlink_metadata(project)?;
    if !project_metadata.is_dir() || project_metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "Aube install is not a real directory: {}",
            project.display()
        ));
    }
    let canonical_project = dunce::canonicalize(project)
        .with_context(|| format!("canonicalizing Aube install {}", project.display()))?;
    let root_manifest_path = canonical_project.join("package.json");
    let root_manifest = read_bounded_regular_json(&root_manifest_path, "Aube root manifest")?;
    let dependencies = root_manifest
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow!("Aube root manifest has no dependencies object"))?;
    if root_manifest
        .get("name")
        .and_then(serde_json::Value::as_str)
        != Some("aube-global")
        || root_manifest
            .get("version")
            .and_then(serde_json::Value::as_str)
            != Some("0.0.0")
        || root_manifest
            .get("private")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        || dependencies.len() != 1
        || dependencies
            .get(package)
            .and_then(serde_json::Value::as_str)
            != Some(version)
    {
        return Err(anyhow!(
            "Aube root manifest does not describe exactly {package}@{version}"
        ));
    }

    let package_manifest_path = canonical_project
        .join("node_modules")
        .join(package)
        .join("package.json");
    let package_manifest =
        read_bounded_regular_json(&package_manifest_path, "installed npm package manifest")?;
    let canonical_manifest = dunce::canonicalize(&package_manifest_path)?;
    let canonical_package_dir = canonical_manifest
        .parent()
        .ok_or_else(|| anyhow!("installed package manifest has no parent"))?;
    if !canonical_package_dir.starts_with(&canonical_project)
        || !canonical_manifest.starts_with(canonical_package_dir)
        || package_manifest
            .get("name")
            .and_then(serde_json::Value::as_str)
            != Some(package)
        || package_manifest
            .get("version")
            .and_then(serde_json::Value::as_str)
            != Some(version)
    {
        return Err(anyhow!(
            "installed package manifest does not match {package}@{version}"
        ));
    }
    validate_aube_lock_identity(&canonical_project.join("aube-lock.yaml"), package, version)
}

fn read_bounded_regular_json(path: &Path, description: &str) -> Result<serde_json::Value> {
    const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading {description} metadata at {}", path.display()))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_MANIFEST_BYTES
    {
        return Err(anyhow!(
            "{description} is not a bounded regular file: {}",
            path.display()
        ));
    }
    serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("parsing {description} {}", path.display()))
}

fn validate_aube_lock_identity(path: &Path, package: &str, version: &str) -> Result<()> {
    const MAX_LOCK_BYTES: u64 = 64 * 1024 * 1024;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading Aube lock metadata at {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_LOCK_BYTES {
        return Err(anyhow!(
            "Aube lock is not a bounded regular file: {}",
            path.display()
        ));
    }
    let value: serde_yaml::Value = serde_yaml::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("parsing Aube lock {}", path.display()))?;
    if yaml_lock_major(
        value
            .get("lockfileVersion")
            .ok_or_else(|| anyhow!("{} is missing lockfileVersion", path.display()))?,
    )? != 9
    {
        return Err(anyhow!("unsupported Aube global lock format"));
    }
    let dependencies = value
        .get("importers")
        .and_then(|value| value.get("."))
        .and_then(|value| value.get("dependencies"))
        .and_then(serde_yaml::Value::as_mapping)
        .ok_or_else(|| anyhow!("Aube lock has no root dependency map"))?;
    if dependencies.len() != 1 {
        return Err(anyhow!(
            "Aube lock does not contain exactly one root dependency"
        ));
    }
    let dependency_key = serde_yaml::Value::String(package.into());
    let dependency = dependencies
        .get(&dependency_key)
        .ok_or_else(|| anyhow!("Aube lock is missing {package}"))?;
    let specifier = dependency
        .get("specifier")
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or_default();
    let resolved = dependency
        .get("version")
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or_default();
    if specifier != version
        || !(resolved == version
            || resolved
                .strip_prefix(version)
                .is_some_and(|suffix| suffix.starts_with('(')))
    {
        return Err(anyhow!(
            "Aube lock dependency identity mismatch for {package}@{version}"
        ));
    }
    let package_key = serde_yaml::Value::String(format!("{package}@{version}"));
    let integrity = value
        .get("packages")
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|packages| packages.get(&package_key))
        .and_then(|record| record.get("resolution"))
        .and_then(|resolution| resolution.get("integrity"))
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or_default();
    if integrity.trim().is_empty() {
        return Err(anyhow!(
            "Aube lock is missing integrity for {package}@{version}"
        ));
    }
    Ok(())
}

fn validate_owned_stage_directory(stage_root: &Path, directory: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "Aube staging component is not a real directory: {}",
            directory.display()
        ));
    }
    let stage = dunce::canonicalize(stage_root)?;
    let directory = dunce::canonicalize(directory)?;
    if directory == stage || !directory.starts_with(&stage) {
        return Err(anyhow!(
            "Aube staging component escapes {}: {}",
            stage.display(),
            directory.display()
        ));
    }
    Ok(())
}

fn normalize_global_bins(
    backend: &NpmPackageBackend,
    _version: &ToolVersion,
    installer: NpmInstaller,
    layout: &GlobalInstallLayout,
) -> Result<()> {
    // Resolve all package-manager launchers to relocatable osdk-owned
    // launchers. Native npm/pnpm wrappers commonly embed the staging prefix.
    let package_json = package_manifest_path(backend, installer, layout).ok_or_else(|| {
        anyhow!(
            "cannot locate {} under staged global install {}",
            backend.package(),
            layout.root.display()
        )
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&package_json)?)?;
    let entries = package_bin_entries(&manifest, backend.package())?;
    let package_dir = package_json
        .parent()
        .ok_or_else(|| anyhow!("package manifest has no parent"))?;
    let mut targets = Vec::with_capacity(entries.len());
    for (name, relative_target) in entries {
        let canonical_target = dunce::canonicalize(package_dir.join(relative_target))?;
        targets.push((name, canonical_target));
    }
    reset_global_bin_dir(layout)?;
    for (name, canonical_target) in targets {
        let target_relative = path_relative_to(&canonical_target, &layout.bin)?;
        #[cfg(unix)]
        {
            let destination = layout.bin.join(&name);
            let _ = std::fs::remove_file(&destination);
            std::os::unix::fs::symlink(target_relative, destination)?;
        }
        #[cfg(windows)]
        {
            if name.contains(['%', '!', '^', '&', '|', '<', '>', '(', ')']) {
                return Err(anyhow!("unsafe npm bin name `{name}` for cmd wrapper"));
            }
            let destination = layout.bin.join(format!("{name}.cmd"));
            std::fs::write(destination, render_windows_node_wrapper(&target_relative)?)?;
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn render_windows_node_wrapper(target_relative: &Path) -> Result<String> {
    let relative = target_relative.to_string_lossy().replace('/', "\\");
    if relative.contains(['%', '!', '\r', '\n', '\0']) {
        return Err(anyhow!(
            "unsafe Windows npm launcher target `{relative}` contains cmd expansion characters"
        ));
    }
    Ok(format!(
        "@echo off\r\nsetlocal DisableDelayedExpansion\r\nnode \"%~dp0{relative}\" %*\r\n"
    ))
}

fn reset_global_bin_dir(layout: &GlobalInstallLayout) -> Result<()> {
    if layout.bin == layout.root {
        for entry in std::fs::read_dir(&layout.root)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.is_file() || metadata.file_type().is_symlink() {
                remove_path(&entry.path())?;
            }
        }
    } else {
        remove_path(&layout.bin)?;
        std::fs::create_dir_all(&layout.bin)?;
    }
    Ok(())
}

fn path_relative_to(target: &Path, base: &Path) -> Result<PathBuf> {
    let target = target.components().collect::<Vec<_>>();
    let base = dunce::canonicalize(base)?;
    let base = base.components().collect::<Vec<_>>();
    let shared = target
        .iter()
        .zip(&base)
        .take_while(|(left, right)| left == right)
        .count();
    if shared == 0 {
        return Err(anyhow!("cannot create cross-volume relative launcher"));
    }
    let mut relative = PathBuf::new();
    for _ in shared..base.len() {
        relative.push("..");
    }
    for component in &target[shared..] {
        relative.push(component.as_os_str());
    }
    Ok(relative)
}

async fn run_native_installer(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
    layout: &GlobalInstallLayout,
    selected_registry: Option<&str>,
) -> Result<()> {
    let (_, manager_version, executable) = runtime
        .manager
        .as_ref()
        .ok_or_else(|| anyhow!("managed {installer} was not prepared"))?;
    let manager = match installer {
        NpmInstaller::Npm => PackageManager::Npm,
        NpmInstaller::Pnpm => PackageManager::Pnpm,
        _ => unreachable!(),
    };
    let package_spec = format!("{}@{}", backend.package(), version.version);
    let args = native_args(
        installer,
        &layout.root,
        &layout.bin,
        &package_spec,
        version,
        &app.ctx.dirs,
        app.ctx.config.settings.offline,
    )?;
    let mut env = isolated_native_env(
        app,
        &layout.root,
        &layout.bin,
        installer,
        &runtime.node_bin,
        manager_version,
    )?;
    if let Some(url) = selected_registry {
        env.insert(package_registry::registry_env(manager).into(), url.into());
    }
    run_managed_command(executable, &args, &env, &layout.root)
}

fn global_registry_ctx(app: &App) -> Result<osdk_core::backend::Ctx> {
    let mut config = osdk_core::config::Config::load_user(&app.ctx.dirs.user_config_file())?;
    // Command-line offline mode is already folded into the active context and
    // must remain authoritative even though project configuration is excluded.
    config.settings.offline = app.ctx.config.settings.offline;
    Ok(osdk_core::backend::Ctx {
        dirs: app.ctx.dirs.clone(),
        platform: app.ctx.platform,
        config,
        client: app.ctx.client.clone(),
        cas: app.ctx.cas.clone(),
        show_progress: app.ctx.show_progress,
    })
}

async fn plan_isolated_global_registry(
    app: &App,
    cwd: &Path,
    manager: PackageManager,
    executable_alias: &str,
    logical_args: &[String],
) -> Result<Option<String>> {
    let context = global_registry_ctx(app)?;
    match package_registry::plan(
        &context,
        cwd,
        manager,
        executable_alias,
        logical_args,
        |key| std::env::var(key).ok(),
    )
    .await?
    {
        RegistryPlan::Selected { url, .. } => Ok(Some(url)),
        RegistryPlan::Unavailable { probes } => Err(unavailable_registry_error(manager, &probes)),
        RegistryPlan::PassThrough { .. } if context.config.settings.offline => Ok(None),
        RegistryPlan::PassThrough { reason } => Err(anyhow!(osdk_core::t!(
            "err.npm_global_registry_isolation",
            manager = manager,
            reason = reason
        ))),
    }
}

fn native_args(
    installer: NpmInstaller,
    install_root: &Path,
    bin_dir: &Path,
    package_spec: &str,
    version: &ToolVersion,
    dirs: &osdk_core::dirs::Dirs,
    offline: bool,
) -> Result<Vec<String>> {
    let build_policy = npm_build_policy(version)?;
    match installer {
        NpmInstaller::Npm => {
            let mut args = vec![
                "install".into(),
                "--global".into(),
                "--prefix".into(),
                install_root.display().to_string(),
                "--audit=false".into(),
                "--fund=false".into(),
            ];
            match build_policy {
                NpmBuildPolicy::Deny => args.push("--ignore-scripts".into()),
                NpmBuildPolicy::AllowAll => {}
                NpmBuildPolicy::Packages(_) => {
                    return Err(anyhow!(
                        "installer `npm` cannot enforce a package allowlist; use allow_builds=false or true"
                    ))
                }
            }
            if offline {
                args.push("--offline".into());
            }
            args.push(package_spec.into());
            Ok(args)
        }
        NpmInstaller::Pnpm => {
            let mut args = vec![
                "add".into(),
                "--global".into(),
                "--global-dir".into(),
                install_root.join("pnpm-global").display().to_string(),
                "--global-bin-dir".into(),
                bin_dir.display().to_string(),
                "--store-dir".into(),
                dirs.store.join("pnpm-store").display().to_string(),
            ];
            match build_policy {
                NpmBuildPolicy::Deny => args.push("--ignore-scripts".into()),
                NpmBuildPolicy::AllowAll => args.push("--dangerously-allow-all-builds".into()),
                NpmBuildPolicy::Packages(packages) => {
                    for package in packages {
                        args.push(format!("--allow-build={package}"));
                    }
                }
            }
            if offline {
                args.push("--offline".into());
            }
            args.push(package_spec.into());
            Ok(args)
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
fn native_preflight_args(
    installer: NpmInstaller,
    package_spec: &str,
    offline: bool,
) -> Vec<String> {
    let mut args = vec![
        if installer == NpmInstaller::Npm {
            "install".into()
        } else {
            "add".into()
        },
        package_spec.into(),
    ];
    if offline {
        args.push("--offline".into());
    }
    args
}

fn isolated_native_env(
    app: &App,
    install_root: &Path,
    bin_dir: &Path,
    installer: NpmInstaller,
    node_bin: &Path,
    manager_version: &ToolVersion,
) -> Result<BTreeMap<String, String>> {
    let native = install_root.join(NATIVE_CONFIG_DIR);
    std::fs::create_dir_all(&native)?;
    let user_config = native.join("user.npmrc");
    let global_config = native.join("global.npmrc");
    for path in [&user_config, &global_config] {
        if !path.exists() {
            std::fs::write(path, b"")?;
        }
    }
    let manager_dir = runtime_manager_dir(app, installer, manager_version)?;
    let mut path_entries = vec![bin_dir.to_path_buf(), manager_dir, node_bin.to_path_buf()];
    path_entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path_entries)?;
    let cache_root = osdk_core::cache::downstream_root(&app.ctx.dirs.cache);
    let mut env = BTreeMap::from([
        ("PATH".into(), path.to_string_lossy().into_owned()),
        ("HOME".into(), native.join("home").display().to_string()),
        (
            "npm_config_userconfig".into(),
            user_config.display().to_string(),
        ),
        (
            "npm_config_globalconfig".into(),
            global_config.display().to_string(),
        ),
        ("npm_config_update_notifier".into(), "false".into()),
        ("npm_config_audit".into(), "false".into()),
        ("npm_config_fund".into(), "false".into()),
        ("COREPACK_ENABLE_PROJECT_SPEC".into(), "0".into()),
    ]);
    std::fs::create_dir_all(native.join("home"))?;
    match installer {
        NpmInstaller::Npm => {
            env.insert(
                "npm_config_prefix".into(),
                install_root.display().to_string(),
            );
            env.insert(
                "npm_config_cache".into(),
                cache_root.join("npm").display().to_string(),
            );
        }
        NpmInstaller::Pnpm => {
            env.insert("PNPM_HOME".into(), bin_dir.display().to_string());
            env.insert(
                "pnpm_config_cache_dir".into(),
                cache_root.join("pnpm").display().to_string(),
            );
            env.insert(
                "pnpm_config_store_dir".into(),
                app.ctx.dirs.store.join("pnpm-store").display().to_string(),
            );
            env.insert(
                "npm_config_store_dir".into(),
                app.ctx.dirs.store.join("pnpm-store").display().to_string(),
            );
            env.insert(
                "pnpm_config_state_dir".into(),
                app.ctx
                    .dirs
                    .data
                    .join("npm-native-state/pnpm")
                    .display()
                    .to_string(),
            );
        }
        _ => unreachable!(),
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "TMPDIR",
        "TMP",
        "TEMP",
        "SystemRoot",
        "WINDIR",
        "ComSpec",
        "PATHEXT",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    Ok(env)
}

fn runtime_manager_dir(
    app: &App,
    installer: NpmInstaller,
    manager_version: &ToolVersion,
) -> Result<PathBuf> {
    let backend = app.registry.get(installer.as_str())?;
    backend
        .bin_paths(&app.ctx, manager_version)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("managed {installer} has no bin directory"))
}

fn run_managed_command(
    executable: &Path,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<()> {
    let mut command = if cfg!(windows)
        && executable
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
            }) {
        let mut command = Command::new(
            std::env::var_os("ComSpec").unwrap_or_else(|| std::ffi::OsString::from("cmd.exe")),
        );
        command.args(["/D", "/S", "/C", "call"]).arg(executable);
        command
    } else {
        Command::new(executable)
    };
    let output = command
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running managed installer {}", executable.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
        let stdout = &output.stdout[..output.stdout.len().min(MAX_DIAGNOSTIC_BYTES)];
        let stderr = &output.stderr[..output.stderr.len().min(MAX_DIAGNOSTIC_BYTES)];
        Err(anyhow!(
            "managed installer {} failed with {}:\nstdout:\n{}\nstderr:\n{}",
            executable.display(),
            output.status,
            String::from_utf8_lossy(stdout),
            String::from_utf8_lossy(stderr)
        ))
    }
}

fn native_lock_path(layout: &GlobalInstallLayout, installer: NpmInstaller) -> Option<PathBuf> {
    match installer {
        NpmInstaller::Aube => Some(layout.project.join("aube-lock.yaml")),
        NpmInstaller::Npm => None,
        NpmInstaller::Pnpm => find_lockfile(&layout.root.join("pnpm-global"), "pnpm-lock.yaml"),
        NpmInstaller::Auto => unreachable!(),
    }
}

fn find_lockfile(root: &Path, file_name: &str) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(4)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == file_name)
        .map(|entry| entry.into_path())
        .min()
}

fn validate_global_package_identity(
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    layout: &GlobalInstallLayout,
) -> Result<()> {
    let package_json = package_manifest_path(backend, installer, layout).ok_or_else(|| {
        anyhow!(
            "global installer `{installer}` did not contain {}@{} under {}",
            backend.package(),
            version.version,
            layout.root.display()
        )
    })?;
    let bytes = std::fs::read(&package_json)
        .with_context(|| format!("reading installed package {}", package_json.display()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing installed package {}", package_json.display()))?;
    let actual_name = manifest.get("name").and_then(serde_json::Value::as_str);
    let actual_version = manifest.get("version").and_then(serde_json::Value::as_str);
    if actual_name != Some(backend.package()) || actual_version != Some(version.version.as_str()) {
        return Err(anyhow!(
            "installed global package identity mismatch: expected {}@{}, found {}@{}",
            backend.package(),
            version.version,
            actual_name.unwrap_or("<missing>"),
            actual_version.unwrap_or("<missing>")
        ));
    }
    validate_declared_package_bins(backend.package(), &package_json, &manifest, &layout.bin)?;
    Ok(())
}

fn package_manifest_path(
    backend: &NpmPackageBackend,
    installer: NpmInstaller,
    layout: &GlobalInstallLayout,
) -> Option<PathBuf> {
    Some(match installer {
        NpmInstaller::Aube => layout
            .project
            .join("node_modules")
            .join(backend.package())
            .join("package.json"),
        NpmInstaller::Npm => {
            #[cfg(windows)]
            let modules = layout.root.join("node_modules");
            #[cfg(not(windows))]
            let modules = layout.root.join("lib/node_modules");
            modules.join(backend.package()).join("package.json")
        }
        NpmInstaller::Pnpm => {
            find_package_json(&layout.root.join("pnpm-global"), backend.package())?
        }
        NpmInstaller::Auto => unreachable!(),
    })
}

fn validate_declared_package_bins(
    package: &str,
    manifest_path: &Path,
    manifest: &serde_json::Value,
    bin_dir: &Path,
) -> Result<()> {
    let package_dir = manifest_path.parent().ok_or_else(|| {
        anyhow!(
            "package manifest has no parent: {}",
            manifest_path.display()
        )
    })?;
    let entries = package_bin_entries(manifest, package)?;
    let canonical_package = dunce::canonicalize(package_dir)?;
    for (name, target) in entries {
        if target.is_absolute()
            || target.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(anyhow!(
                "npm package {package} declares unsafe bin path {}",
                target.display()
            ));
        }
        let target = package_dir.join(target);
        let canonical_target = dunce::canonicalize(&target)?;
        if !canonical_target.is_file() || !canonical_target.starts_with(&canonical_package) {
            return Err(anyhow!(
                "npm package {package} bin `{name}` escapes {}",
                package_dir.display()
            ));
        }
        let _launcher = global_bin_entry(bin_dir, &name).ok_or_else(|| {
            anyhow!(
                "global npm package {package} did not publish declared bin `{name}` under {}",
                bin_dir.display()
            )
        })?;
        #[cfg(unix)]
        {
            let metadata = std::fs::symlink_metadata(&_launcher)?;
            if !metadata.file_type().is_symlink()
                || dunce::canonicalize(&_launcher)? != canonical_target
            {
                return Err(anyhow!(
                    "global launcher `{name}` does not resolve to the target declared by {package}"
                ));
            }
        }
        #[cfg(windows)]
        {
            let extension = _launcher
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_ascii_lowercase);
            if !matches!(extension.as_deref(), Some("cmd")) {
                return Err(anyhow!(
                    "global launcher `{name}` for {package} is an opaque Windows executable"
                ));
            }
            for shadow in [
                bin_dir.join(format!("{name}.exe")),
                bin_dir.join(name.as_str()),
            ] {
                if shadow.exists() {
                    return Err(anyhow!(
                        "global launcher `{name}` for {package} has an opaque Windows shadow"
                    ));
                }
            }
            let actual = parse_windows_node_wrapper(&_launcher)?;
            if dunce::canonicalize(&actual)? != canonical_target {
                return Err(anyhow!(
                    "global launcher `{name}` does not execute the target declared by {package}"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn parse_windows_node_wrapper(path: &Path) -> Result<PathBuf> {
    const MAX_WRAPPER_BYTES: u64 = 64 * 1024;
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_WRAPPER_BYTES {
        return Err(anyhow!("unsafe Windows npm wrapper {}", path.display()));
    }
    let text = std::fs::read_to_string(path)?;
    let mut target = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty()
            || line.eq_ignore_ascii_case("@echo off")
            || line.starts_with("::")
            || line.to_ascii_lowercase().starts_with("rem ")
            || line.to_ascii_lowercase().starts_with("setlocal")
            || line.to_ascii_lowercase().starts_with("endlocal")
        {
            continue;
        }
        if line.contains(['&', '|', '>', '<', '`', '!']) {
            return Err(anyhow!("unsafe Windows npm wrapper {}", path.display()));
        }
        let line = line.strip_prefix('@').unwrap_or(line).trim_start();
        let lower = line.to_ascii_lowercase();
        let command_end = if lower.starts_with("node.exe ") {
            8
        } else if lower.starts_with("node ") {
            4
        } else {
            return Err(anyhow!(
                "unrecognized Windows npm wrapper {}",
                path.display()
            ));
        };
        let arguments = line[command_end..].trim_start();
        let raw_target = arguments
            .strip_prefix('\"')
            .and_then(|quoted| quoted.split_once('\"').map(|(target, _)| target))
            .ok_or_else(|| anyhow!("unquoted Windows npm target in {}", path.display()))?;
        if raw_target
            .strip_prefix("%~dp0")
            .is_some_and(|relative| relative.contains('%'))
        {
            return Err(anyhow!("unsafe Windows npm wrapper {}", path.display()));
        }
        let relative = raw_target
            .strip_prefix("%~dp0")
            .ok_or_else(|| anyhow!("non-relative Windows npm target in {}", path.display()))?;
        if relative.contains(['\r', '\n', '\0']) {
            return Err(anyhow!("unsafe Windows npm wrapper {}", path.display()));
        }
        let candidate = path
            .parent()
            .unwrap_or(Path::new(""))
            .join(relative.trim_start_matches(['/', '\\']));
        if target.replace(candidate).is_some() {
            return Err(anyhow!(
                "multiple commands in Windows npm wrapper {}",
                path.display()
            ));
        }
    }
    target.ok_or_else(|| anyhow!("missing command in Windows npm wrapper {}", path.display()))
}

fn package_bin_entries(
    manifest: &serde_json::Value,
    package: &str,
) -> Result<Vec<(String, PathBuf)>> {
    let mut entries = match manifest.get("bin") {
        Some(serde_json::Value::String(path)) => vec![(
            package.rsplit('/').next().unwrap_or(package).to_string(),
            PathBuf::from(path),
        )],
        Some(serde_json::Value::Object(entries)) => entries
            .iter()
            .filter_map(|(name, path)| {
                path.as_str()
                    .map(|path| (name.clone(), PathBuf::from(path)))
            })
            .collect(),
        _ => Vec::new(),
    };
    entries.retain(|(name, _)| {
        !name.is_empty()
            && !name.contains(['/', '\\'])
            && !name.contains(['%', '!', '^', '&', '|', '<', '>', '(', ')'])
    });
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries.dedup_by(|left, right| left.0 == right.0);
    if entries.is_empty() {
        return Err(anyhow!(
            "npm package {package} declares no safe executable bins"
        ));
    }
    Ok(entries)
}

fn global_bin_entry(bin_dir: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        bin_dir.join(format!("{name}.cmd")),
        bin_dir.join(format!("{name}.exe")),
        bin_dir.join(format!("{name}.bat")),
        bin_dir.join(name),
    ];
    #[cfg(not(windows))]
    let candidates = [bin_dir.join(name)];
    candidates.into_iter().find(|candidate| candidate.exists())
}

fn find_package_json(root: &Path, package: &str) -> Option<PathBuf> {
    let suffix = format!("node_modules/{package}/package.json").replace('\\', "/");
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "package.json")
        .map(|entry| entry.into_path())
        .filter(|path| path.to_string_lossy().replace('\\', "/").ends_with(&suffix))
        .min()
}

fn manifest_bin_names(root: &Path) -> Result<Vec<String>> {
    let manifest =
        osdk_core::inventory::DynamicToolManifest::load(root).map_err(anyhow::Error::new)?;
    Ok(manifest.bins.into_iter().map(|bin| bin.name).collect())
}

fn manifest_bin_names_best_effort(root: &Path) -> Vec<String> {
    if !osdk_core::inventory::DynamicToolManifest::manifest_path(root).is_file() {
        return Vec::new();
    }
    manifest_bin_names(root).unwrap_or_default()
}

fn selected_global_bin_names(
    ctx: &osdk_core::backend::Ctx,
    backend: &NpmPackageBackend,
    incoming_root: &Path,
) -> Result<Vec<String>> {
    let mut names = manifest_bin_names_best_effort(incoming_root);
    let config = osdk_core::config::Config::load_user(&ctx.dirs.user_config_file())
        .context("reloading global config before npm shim publication")?;
    let selected = match config.global_tool_configs.get(backend.id()) {
        Some(entry) => selected_global_version(ctx, backend, entry)?,
        None => None,
    };
    if let Some(version) = selected {
        let root = backend.global_install_root_for(ctx, &version)?;
        names.extend(manifest_bin_names_best_effort(&root));
        if let Some(legacy_root) = backend.legacy_global_install_root(ctx, &version)? {
            names.extend(manifest_bin_names_best_effort(&legacy_root));
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn selected_global_version(
    ctx: &osdk_core::backend::Ctx,
    backend: &NpmPackageBackend,
    configured: &osdk_core::config::ToolConfigEntry,
) -> Result<Option<ToolVersion>> {
    let spec = VersionSpec::parse(configured.version());
    let lock_path = ctx.dirs.user_lock_file();
    if lock_path.is_file() {
        if let Some(request) = crate::lockfile::locked_requests(&lock_path, ctx.platform)?
            .into_iter()
            .flatten()
            .find(|request| {
                request.backend == backend.id()
                    && request
                        .options
                        .get(LOCKED_NPM_SCOPE_OPTION)
                        .map(String::as_str)
                        == Some(ToolScope::Global.as_str())
            })
        {
            if let VersionSpec::Exact(version) = request.spec {
                let mut selected = ToolVersion::new(backend.id(), version);
                selected.options = request.options;
                return Ok(Some(selected));
            }
        }
    }
    let mut hint = ToolVersion::new(backend.id(), "scope-selection");
    hint.options = configured.to_request_options();
    hint.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    if let VersionSpec::Exact(version) = &spec {
        hint.version.clone_from(version);
        return Ok(Some(hint));
    }
    let installed = backend.list_installed_for(ctx, &hint)?;
    let candidates = installed
        .iter()
        .map(osdk_core::version::VersionInfo::stable)
        .collect::<Vec<_>>();
    Ok(
        osdk_core::version::select_version(&spec, &candidates).map(|version| {
            hint.version = version.version.clone();
            hint
        }),
    )
}

#[derive(Debug)]
struct NativeLockIdentity {
    kind: &'static str,
    format: String,
    sha256: String,
}

fn read_native_lock(path: &Path, installer: NpmInstaller) -> Result<NativeLockIdentity> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading native lock {}", path.display()))?;
    let (kind, format) = match installer {
        NpmInstaller::Aube | NpmInstaller::Pnpm => {
            let value: serde_yaml::Value = serde_yaml::from_slice(&bytes)
                .with_context(|| format!("parsing native lock {}", path.display()))?;
            let major = yaml_lock_major(
                value
                    .get("lockfileVersion")
                    .ok_or_else(|| anyhow!("{} is missing lockfileVersion", path.display()))?,
            )?;
            let kind = if installer == NpmInstaller::Aube {
                "aube"
            } else {
                "pnpm"
            };
            (kind, format!("{kind}-v{major}"))
        }
        NpmInstaller::Npm => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing native lock {}", path.display()))?;
            let major = value
                .get("lockfileVersion")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow!("{} is missing numeric lockfileVersion", path.display()))?;
            ("npm", format!("package-lock-v{major}"))
        }
        NpmInstaller::Auto => unreachable!(),
    };
    let supported = match installer {
        NpmInstaller::Aube | NpmInstaller::Pnpm => format.ends_with("-v9"),
        NpmInstaller::Npm => matches!(format.as_str(), "package-lock-v2" | "package-lock-v3"),
        NpmInstaller::Auto => unreachable!(),
    };
    if !supported {
        anyhow::bail!(
            "unsupported native lock format `{format}` produced by global installer `{installer}`"
        );
    }
    Ok(NativeLockIdentity {
        kind,
        format,
        sha256: osdk_core::pipeline::verify::hash_bytes(&bytes, HashAlgo::Sha256),
    })
}

fn yaml_lock_major(value: &serde_yaml::Value) -> Result<u64> {
    let raw = match value {
        serde_yaml::Value::String(value) => value.clone(),
        serde_yaml::Value::Number(value) => value.to_string(),
        _ => return Err(anyhow!("native lockfileVersion must be a string or number")),
    };
    raw.trim()
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| anyhow!("native lockfileVersion is malformed"))
}

fn inject_native_metadata(
    version: &mut ToolVersion,
    installer: NpmInstaller,
    node_version: &str,
    native: Option<&NativeLockIdentity>,
) {
    version.options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        installer.as_str().into(),
    );
    version.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    version
        .options
        .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), node_version.into());
    for key in [
        LOCKED_NPM_NATIVE_LOCK_KIND_OPTION,
        LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION,
        LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION,
    ] {
        version.options.remove(key);
    }
    if let Some(native) = native {
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_KIND_OPTION.into(),
            native.kind.into(),
        );
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_FORMAT_OPTION.into(),
            native.format.clone(),
        );
        version.options.insert(
            LOCKED_NPM_NATIVE_LOCK_SHA256_OPTION.into(),
            native.sha256.clone(),
        );
    }
}

fn completed_install_matches_at(
    platform: osdk_core::platform::Platform,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    node_version: &str,
    layout: &GlobalInstallLayout,
    lock_path: Option<&Path>,
) -> Result<bool> {
    let root = &layout.root;
    if !root.join(".osdk-complete").is_file() {
        return Ok(false);
    }
    if installer != NpmInstaller::Npm && lock_path.is_none() {
        return Ok(false);
    }
    let manifest = match osdk_core::inventory::DynamicToolManifest::load(root) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    let receipt = match osdk_core::backend::npm_package::load_npm_receipt(root) {
        Ok(receipt) => receipt,
        Err(_) => return Ok(false),
    };
    // Aube's former synthetic-project implementation used a different root
    // manifest. Requiring the true global-add manifest and graph prevents an
    // apparently complete legacy tree from bypassing the sidecar migration.
    if installer == NpmInstaller::Aube
        && validate_aube_project(&layout.project, backend.package(), &version.version).is_err()
    {
        return Ok(false);
    }
    if validate_global_package_identity(backend, version, installer, layout).is_err() {
        return Ok(false);
    }
    if manifest.bins.is_empty()
        || manifest.bins.iter().any(|bin| {
            let path = root.join(&bin.path);
            let entry_is_file = path.is_file();
            dunce::canonicalize(&path)
                .ok()
                .zip(dunce::canonicalize(root).ok())
                .is_none_or(|(target, root)| {
                    !entry_is_file || !target.is_file() || !target.starts_with(root)
                })
        })
    {
        return Ok(false);
    }
    let expected_native = match lock_path {
        Some(path) if path.is_file() => match read_native_lock(path, installer) {
            Ok(native) => Some(native),
            Err(_) => return Ok(false),
        },
        Some(_) if installer != NpmInstaller::Npm => return Ok(false),
        _ => None,
    };
    let native_matches = match expected_native {
        Some(native) => {
            receipt.native_lock_format.as_ref() == Some(&native.format)
                && receipt.native_lock_sha256.as_ref() == Some(&native.sha256)
        }
        None => receipt.native_lock_format.is_none() && receipt.native_lock_sha256.is_none(),
    };
    let expected_build_policy = npm_build_policy_identity(version)?;
    let build_policy_matches = receipt.build_policy == expected_build_policy;
    let expected_identity = osdk_core::tool::InstallIdentity::new(
        &version.backend,
        &version.version,
        platform.to_string(),
        osdk_core::tool::InstallScope::Global,
        &version.options,
        vec![osdk_core::tool::InstallDependency {
            kind: osdk_core::tool::InstallDependencyKind::Runtime,
            id: "node".into(),
            version: node_version.into(),
            identity: None,
        }],
        BTreeMap::new(),
    )?;
    let bin_dir = &layout.bin;
    Ok(manifest.matches_identity(&expected_identity)
        && receipt.installer == installer.as_str()
        && receipt.node_version == node_version
        && build_policy_matches
        && native_matches
        && bin_dir.is_dir())
}

fn persist_global_lock(
    app: &App,
    request: &ToolRequest,
    version: &ToolVersion,
    runtime: &ManagedRuntime,
) -> Result<()> {
    let path = app.ctx.dirs.user_lock_file();
    let mut resolved = vec![(runtime.node_request.clone(), runtime.node_version.clone())];
    if let Some((manager_request, manager_version, _)) = &runtime.manager {
        resolved.push((manager_request.clone(), manager_version.clone()));
    }
    let locked_request = exact_request(request.clone(), version);
    resolved.push((locked_request, version.clone()));
    crate::lockfile::upsert_resolved_many_with_scope(
        &path,
        app.ctx.platform,
        &app.ctx.dirs,
        &resolved,
        crate::lockfile::LockScope::Global,
    )?;
    Ok(())
}

fn persist_global_config(
    app: &App,
    request: &ToolRequest,
    spec: &str,
    installer: NpmInstaller,
) -> Result<()> {
    let mut options = request
        .options
        .iter()
        .filter(|(key, _)| !key.starts_with("__osdk_"))
        .map(|(key, value)| Ok((key.clone(), option_value(key, value)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    options.insert(
        INSTALLER_OPTION.into(),
        osdk_core::config::ToolConfigValue::String(installer.as_str().into()),
    );
    crate::config_edit::set_global_tool_config_unlocked(
        &app.ctx,
        &request.backend,
        &osdk_core::config::StructuredToolConfig {
            version: spec.into(),
            options,
        },
    )
}

fn option_value(key: &str, value: &str) -> Result<osdk_core::config::ToolConfigValue> {
    if key == "allow_builds" {
        return Ok(match parse_npm_build_policy(Some(value))? {
            NpmBuildPolicy::Deny => osdk_core::config::ToolConfigValue::Bool(false),
            NpmBuildPolicy::AllowAll => osdk_core::config::ToolConfigValue::Bool(true),
            NpmBuildPolicy::Packages(packages) => {
                osdk_core::config::ToolConfigValue::Array(packages)
            }
        });
    }
    Ok(osdk_core::config::ToolConfigValue::String(value.into()))
}

fn apply_source_override(app: &mut App, tool: &str) {
    if let Some(id) = app.source_override.clone() {
        app.ctx
            .config
            .sources
            .per_tool
            .entry(tool.into())
            .or_default()
            .pin = Some(id);
    }
}

fn expand_alias(app: &App, request: &ToolRequest) -> Result<ToolRequest> {
    let mut effective = request.clone();
    effective.spec = VersionSpec::parse(
        &app.ctx
            .config
            .expand_alias(&request.backend, &request.spec.to_string())?,
    );
    Ok(effective)
}

fn find_bin_dir(
    ctx: &osdk_core::backend::Ctx,
    backend: &dyn Backend,
    version: &ToolVersion,
    executable: &str,
) -> Result<PathBuf> {
    backend
        .bin_paths(ctx, version)?
        .into_iter()
        .find(|path| find_executable(std::slice::from_ref(path), executable).is_some())
        .ok_or_else(|| anyhow!("managed {executable} executable was not installed"))
}

fn find_executable(paths: &[PathBuf], name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
        name.into(),
    ];
    #[cfg(not(windows))]
    let candidates = [name.to_string()];
    paths.iter().find_map(|directory| {
        candidates
            .iter()
            .map(|candidate| directory.join(candidate))
            .find(|candidate| candidate.is_file())
    })
}

fn unavailable_registry_error(manager: PackageManager, probes: &[RegistryProbe]) -> anyhow::Error {
    let details = probes
        .iter()
        .map(|probe| {
            format!(
                "{} ({})",
                probe.url,
                probe.error.as_deref().unwrap_or("unreachable")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    anyhow!("no usable {manager} registry; manager was not started: {details}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::ffi::OsString;

    #[test]
    fn native_lock_identity_reads_all_supported_formats() {
        let temporary = tempfile::tempdir().unwrap();
        let cases = [
            (
                NpmInstaller::Aube,
                "aube-lock.yaml",
                "lockfileVersion: '9.0'\n",
                "aube-v9",
            ),
            (
                NpmInstaller::Pnpm,
                "pnpm-lock.yaml",
                "lockfileVersion: '9.0'\n",
                "pnpm-v9",
            ),
            (
                NpmInstaller::Npm,
                "package-lock.json",
                "{\"lockfileVersion\":3}",
                "package-lock-v3",
            ),
        ];
        for (installer, name, contents, format) in cases {
            let path = temporary.path().join(name);
            std::fs::write(&path, contents).unwrap();
            let identity = read_native_lock(&path, installer).unwrap();
            assert_eq!(identity.format, format);
            assert_eq!(identity.sha256.len(), 64);
        }
    }

    #[test]
    fn aube_global_arguments_enforce_relocatable_build_policy() {
        let base = ToolVersion::new("npm:fixture-cli", "1.2.3");
        let denied = aube_global_args(&base, "fixture-cli@1.2.3".into(), None).unwrap();
        assert!(denied.iter().any(|arg| arg == "--deny-build=*"));
        assert!(!denied.iter().any(|arg| arg == "--ignore-scripts"));
        assert!(denied.iter().any(|arg| arg == "--disable-gvs"));
        assert!(denied
            .iter()
            .any(|arg| arg == "--config.nodeLinker=hoisted"));

        let mut allow_all = base.clone();
        allow_all
            .options
            .insert("allow_builds".into(), " TRUE ".into());
        let allow_all = aube_global_args(&allow_all, "fixture-cli@1.2.3".into(), None).unwrap();
        assert!(allow_all
            .iter()
            .any(|arg| arg == "--dangerously-allow-all-builds"));
        assert!(!allow_all.iter().any(|arg| arg.starts_with("--deny-build")));

        let mut selected = base;
        selected.options.insert(
            "allow_builds".into(),
            " @Scope/Native, Plain-Native ".into(),
        );
        let selected = aube_global_args(
            &selected,
            "fixture-cli@1.2.3".into(),
            Some("https://registry.example.test/".into()),
        )
        .unwrap();
        assert!(selected
            .iter()
            .any(|arg| arg == "--allow-build=@scope/native"));
        assert!(selected
            .iter()
            .any(|arg| arg == "--allow-build=plain-native"));
        assert!(selected
            .iter()
            .any(|arg| arg == "--registry=https://registry.example.test/"));
    }

    #[cfg(unix)]
    fn write_valid_aube_candidate(root: &Path, hash: char, package: &str, version: &str) {
        let install = root.join(format!("fixture-{hash}"));
        let package_dir = install.join("node_modules").join(package);
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(
            install.join("package.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "aube-global",
                "version": "0.0.0",
                "private": true,
                "dependencies": { package: version }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            install.join("aube-lock.yaml"),
            format!(
                "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      {package}:\n        specifier: {version}\n        version: {version}\npackages:\n  {package}@{version}:\n    resolution: {{integrity: sha512-Zml4dHVyZQ==}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            package_dir.join("package.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": package,
                "version": version,
                "bin": { "fixture-cli": "cli.js" }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(package_dir.join("cli.js"), "fixture").unwrap();
        std::os::unix::fs::symlink(&install, root.join(hash.to_string().repeat(64))).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn aube_global_resolver_requires_one_contained_exact_hash_pointer() {
        let temporary = tempfile::tempdir().unwrap();
        let stage = temporary.path().join("stage");
        let package_root = stage.join("aube-global/global-aube");
        std::fs::create_dir_all(&package_root).unwrap();

        let missing =
            resolve_aube_global_install(&stage, &package_root, "fixture-cli", "1.2.3").unwrap_err();
        assert!(missing
            .to_string()
            .contains("exactly one valid hash pointer"));

        write_valid_aube_candidate(&package_root, 'a', "fixture-cli", "1.2.3");
        let selected =
            resolve_aube_global_install(&stage, &package_root, "fixture-cli", "1.2.3").unwrap();
        assert_eq!(
            selected.install_dir,
            dunce::canonicalize(package_root.join("fixture-a")).unwrap()
        );

        write_valid_aube_candidate(&package_root, 'b', "fixture-cli", "1.2.3");
        let ambiguous =
            resolve_aube_global_install(&stage, &package_root, "fixture-cli", "1.2.3").unwrap_err();
        assert!(ambiguous.to_string().contains("ambiguous"));
    }

    #[cfg(unix)]
    #[test]
    fn aube_global_resolver_rejects_escape_and_wrong_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let stage = temporary.path().join("stage");
        let package_root = stage.join("aube-global/global-aube");
        std::fs::create_dir_all(&package_root).unwrap();
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, package_root.join("c".repeat(64))).unwrap();
        let escaped =
            resolve_aube_global_install(&stage, &package_root, "fixture-cli", "1.2.3").unwrap_err();
        assert!(escaped.to_string().contains("escapes the Aube global root"));

        std::fs::remove_file(package_root.join("c".repeat(64))).unwrap();
        write_valid_aube_candidate(&package_root, 'd', "fixture-cli", "9.9.9");
        let mismatch =
            resolve_aube_global_install(&stage, &package_root, "fixture-cli", "1.2.3").unwrap_err();
        assert!(mismatch.to_string().contains("does not describe exactly"));
    }

    #[test]
    fn windows_wrapper_rejects_cmd_expansion_in_target_path() {
        for path in [r#"..\package%PATH%\cli.js"#, r#"..\package!TEMP!\cli.js"#] {
            let error = render_windows_node_wrapper(Path::new(path)).unwrap_err();
            assert!(error.to_string().contains("cmd expansion characters"));
        }
        let wrapper = render_windows_node_wrapper(Path::new(r#"..\package\cli.js"#)).unwrap();
        assert!(wrapper.contains("setlocal DisableDelayedExpansion"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_wrapper_parser_rejects_expanding_targets() {
        let temporary = tempfile::tempdir().unwrap();
        for (name, target) in [
            ("percent.cmd", r#"..\package%PATH%\cli.js"#),
            ("bang.cmd", r#"..\package!TEMP!\cli.js"#),
        ] {
            let wrapper = temporary.path().join(name);
            std::fs::write(
                &wrapper,
                format!("@echo off\r\nnode \"%~dp0{target}\" %*\r\n"),
            )
            .unwrap();
            assert!(parse_windows_node_wrapper(&wrapper)
                .unwrap_err()
                .to_string()
                .contains("unsafe Windows npm wrapper"));
        }
    }

    #[test]
    fn failed_staged_reinstall_preserves_existing_install() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        let backend = NpmPackageBackend::from_id("npm:fixture-cli").unwrap();
        let mut version = ToolVersion::new("npm:fixture-cli", "1.0.0");
        version
            .options
            .insert(INSTALLER_OPTION.into(), NpmInstaller::Npm.as_str().into());
        let layout = write_valid_npm_global_install(&final_root, b"old");
        rewrite_manifest_option_identity(&final_root, &version.options);
        assert!(completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Npm,
            "22.1.0",
            &layout,
            None,
        )
        .unwrap());

        let staged = StagedInstall::begin(&final_root).unwrap();
        let stage_root = staged.root().to_path_buf();
        assert_eq!(stage_root.parent(), final_root.parent());
        std::fs::write(stage_root.join("payload"), b"partial-new").unwrap();
        let install_error: Result<()> = Err(anyhow!("injected manager failure"));
        assert!(install_error.is_err());
        drop(staged);

        assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), b"old");
        assert!(completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Npm,
            "22.1.0",
            &layout,
            None,
        )
        .unwrap());
        assert!(!stage_root.exists());
        assert!(transaction_debris(final_root.parent().unwrap()).is_empty());
    }

    #[test]
    fn interrupted_promotion_phases_recover_deterministically() {
        for phase in [
            PromotionPhase::Prepared,
            PromotionPhase::OldMoved,
            PromotionPhase::NewPromoted,
            PromotionPhase::Activated,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let final_root = temporary.path().join("npm-global/tool/1.0.0");
            let parent = final_root.parent().unwrap();
            std::fs::create_dir_all(parent).unwrap();
            let stage_root = parent.join(".1.0.0.osdk-stage-crash");
            let backup_root = parent.join(".1.0.0.osdk-backup-crash");
            std::fs::create_dir_all(&stage_root).unwrap();
            std::fs::write(stage_root.join("payload"), b"staged").unwrap();
            if phase == PromotionPhase::Prepared {
                std::fs::create_dir_all(&final_root).unwrap();
                std::fs::write(final_root.join("payload"), b"old").unwrap();
            } else {
                std::fs::create_dir_all(&backup_root).unwrap();
                std::fs::write(backup_root.join("payload"), b"old").unwrap();
                if matches!(
                    phase,
                    PromotionPhase::NewPromoted | PromotionPhase::Activated
                ) {
                    std::fs::create_dir_all(&final_root).unwrap();
                    std::fs::write(final_root.join("payload"), b"new").unwrap();
                }
            }
            write_promotion_journal(
                &promotion_journal_path(&final_root),
                &PromotionJournal {
                    stage_root: stage_root.clone(),
                    backup_root: backup_root.clone(),
                    had_previous: true,
                    phase,
                },
            )
            .unwrap();

            recover_interrupted_promotion(&final_root).unwrap();

            let expected = if matches!(
                phase,
                PromotionPhase::NewPromoted | PromotionPhase::Activated
            ) {
                b"new".as_slice()
            } else {
                b"old".as_slice()
            };
            assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), expected);
            assert!(!stage_root.exists());
            assert!(!backup_root.exists());
            assert!(!promotion_journal_path(&final_root).exists());
            assert!(transaction_debris(parent).is_empty());
        }
    }

    #[test]
    fn interrupted_first_install_recovers_by_phase() {
        for phase in [
            PromotionPhase::Prepared,
            PromotionPhase::NewPromoted,
            PromotionPhase::Activated,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let final_root = temporary.path().join("npm-global/tool/1.0.0");
            let parent = final_root.parent().unwrap();
            std::fs::create_dir_all(parent).unwrap();
            let stage_root = parent.join(".1.0.0.osdk-stage-first");
            let backup_root = parent.join(".1.0.0.osdk-backup-first");
            std::fs::create_dir_all(&stage_root).unwrap();
            if phase != PromotionPhase::Prepared {
                std::fs::create_dir_all(&final_root).unwrap();
                std::fs::write(final_root.join("payload"), b"new").unwrap();
            }
            write_promotion_journal(
                &promotion_journal_path(&final_root),
                &PromotionJournal {
                    stage_root,
                    backup_root,
                    had_previous: false,
                    phase,
                },
            )
            .unwrap();

            recover_interrupted_promotion(&final_root).unwrap();

            assert_eq!(
                final_root.exists(),
                matches!(
                    phase,
                    PromotionPhase::NewPromoted | PromotionPhase::Activated
                ),
                "phase {phase:?}"
            );
            assert!(transaction_debris(parent).is_empty());
        }
    }

    #[test]
    fn activation_marker_is_written_only_after_publication_succeeds() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        std::fs::create_dir_all(&final_root).unwrap();
        std::fs::write(final_root.join("payload"), b"old").unwrap();
        let staged = StagedInstall::begin(&final_root).unwrap();
        std::fs::write(staged.root().join("payload"), b"new").unwrap();
        let mut promoted = staged.promote().unwrap();

        let error = publish_then_activate(Some(&promoted), || {
            Err(anyhow!("injected publication crash"))
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected publication crash"));
        let journal = read_promotion_journal(&promotion_journal_path(&final_root))
            .unwrap()
            .unwrap();
        assert_eq!(journal.phase, PromotionPhase::NewPromoted);
        promoted.rollback().unwrap();
    }

    #[test]
    fn new_promoted_recovery_never_restores_old_tree_over_partial_new_metadata() {
        for (config, lock) in [
            (b"old-config".as_slice(), b"old-lock".as_slice()),
            (b"new-config".as_slice(), b"old-lock".as_slice()),
            (b"new-config".as_slice(), b"new-lock".as_slice()),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let final_root = temporary.path().join("npm-global/tool/1.0.0");
            let parent = final_root.parent().unwrap();
            let stage_root = parent.join(".1.0.0.osdk-stage-crash");
            let backup_root = parent.join(".1.0.0.osdk-backup-crash");
            let config_path = temporary.path().join("config.toml");
            let lock_path = temporary.path().join("osdk.lock");
            std::fs::create_dir_all(&final_root).unwrap();
            std::fs::create_dir_all(&stage_root).unwrap();
            std::fs::create_dir_all(&backup_root).unwrap();
            std::fs::write(final_root.join("payload"), b"new").unwrap();
            std::fs::write(stage_root.join("payload"), b"staged").unwrap();
            std::fs::write(backup_root.join("payload"), b"old").unwrap();
            std::fs::write(&config_path, config).unwrap();
            std::fs::write(&lock_path, lock).unwrap();
            write_promotion_journal(
                &promotion_journal_path(&final_root),
                &PromotionJournal {
                    stage_root,
                    backup_root: backup_root.clone(),
                    had_previous: true,
                    phase: PromotionPhase::NewPromoted,
                },
            )
            .unwrap();

            recover_interrupted_promotion(&final_root).unwrap();

            assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), b"new");
            assert_eq!(std::fs::read(config_path).unwrap(), config);
            assert_eq!(std::fs::read(lock_path).unwrap(), lock);
            assert!(!backup_root.exists());
            assert!(!promotion_journal_path(&final_root).exists());
        }
    }

    #[test]
    fn new_promoted_recovery_then_publication_converges_after_each_metadata_boundary() {
        for crash_after in ["promotion", "config", "lock", "shims"] {
            let temporary = tempfile::tempdir().unwrap();
            let final_root = temporary.path().join("npm-global/tool/1.0.0");
            let parent = final_root.parent().unwrap();
            let stage_root = parent.join(".1.0.0.osdk-stage-crash");
            let backup_root = parent.join(".1.0.0.osdk-backup-crash");
            let config_path = temporary.path().join("config.toml");
            let lock_path = temporary.path().join("osdk.lock");
            let shim_path = temporary.path().join("shim");
            std::fs::create_dir_all(&final_root).unwrap();
            std::fs::create_dir_all(&stage_root).unwrap();
            std::fs::create_dir_all(&backup_root).unwrap();
            std::fs::write(final_root.join("payload"), b"new").unwrap();
            std::fs::write(stage_root.join("payload"), b"staged").unwrap();
            std::fs::write(backup_root.join("payload"), b"old").unwrap();
            std::fs::write(&config_path, b"old-config").unwrap();
            std::fs::write(&lock_path, b"old-lock").unwrap();
            std::fs::write(&shim_path, b"old-shim").unwrap();
            if matches!(crash_after, "config" | "lock" | "shims") {
                std::fs::write(&config_path, b"new-config").unwrap();
            }
            if matches!(crash_after, "lock" | "shims") {
                std::fs::write(&lock_path, b"new-lock").unwrap();
            }
            if crash_after == "shims" {
                std::fs::write(&shim_path, b"new-shim").unwrap();
            }
            write_promotion_journal(
                &promotion_journal_path(&final_root),
                &PromotionJournal {
                    stage_root,
                    backup_root,
                    had_previous: true,
                    phase: PromotionPhase::NewPromoted,
                },
            )
            .unwrap();

            recover_interrupted_promotion(&final_root).unwrap();
            publish_then_activate(None, || {
                std::fs::write(&config_path, b"new-config")?;
                std::fs::write(&lock_path, b"new-lock")?;
                std::fs::write(&shim_path, b"new-shim")?;
                Ok(())
            })
            .unwrap();

            assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), b"new");
            assert_eq!(std::fs::read(config_path).unwrap(), b"new-config");
            assert_eq!(std::fs::read(lock_path).unwrap(), b"new-lock");
            assert_eq!(std::fs::read(shim_path).unwrap(), b"new-shim");
            assert!(transaction_debris(parent).is_empty());
        }
    }

    #[test]
    fn successful_publication_writes_activation_marker_last() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        std::fs::create_dir_all(&final_root).unwrap();
        std::fs::write(final_root.join("payload"), b"old").unwrap();
        let staged = StagedInstall::begin(&final_root).unwrap();
        std::fs::write(staged.root().join("payload"), b"new").unwrap();
        let mut promoted = staged.promote().unwrap();
        let publication_ran = std::cell::Cell::new(false);

        publish_then_activate(Some(&promoted), || {
            publication_ran.set(true);
            let journal = read_promotion_journal(&promotion_journal_path(&final_root))?
                .expect("promotion journal exists during publication");
            assert_eq!(journal.phase, PromotionPhase::NewPromoted);
            Ok(())
        })
        .unwrap();

        assert!(publication_ran.get());
        let journal = read_promotion_journal(&promotion_journal_path(&final_root))
            .unwrap()
            .unwrap();
        assert_eq!(journal.phase, PromotionPhase::Activated);
        promoted.finish();
    }

    #[test]
    fn shim_publication_sync_accepts_created_and_removed_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = test_dirs(temporary.path());
        let shims = dirs.shims();
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::write(shims.join("created"), b"shim").unwrap();

        sync_shim_publication(&dirs, &["created".into(), "removed".into()]).unwrap();
    }

    #[test]
    fn shim_sync_failure_does_not_advance_activation_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        std::fs::create_dir_all(&final_root).unwrap();
        std::fs::write(final_root.join("payload"), b"old").unwrap();
        let staged = StagedInstall::begin(&final_root).unwrap();
        std::fs::write(staged.root().join("payload"), b"new").unwrap();
        let mut promoted = staged.promote().unwrap();
        let dirs = test_dirs(temporary.path());
        std::fs::create_dir_all(dirs.shims().join("not-a-file")).unwrap();

        let error = publish_then_activate(Some(&promoted), || {
            sync_shim_publication(&dirs, &["not-a-file".into()])
        })
        .unwrap_err();

        assert!(error.to_string().contains("not a file"));
        let journal = read_promotion_journal(&promotion_journal_path(&final_root))
            .unwrap()
            .unwrap();
        assert_eq!(journal.phase, PromotionPhase::NewPromoted);
        promoted.rollback().unwrap();
    }

    #[test]
    fn legacy_interrupted_promotion_recovers_the_only_backup() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        let parent = final_root.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        let backup = parent.join(".1.0.0.osdk-backup-old-process");
        let stage = parent.join(".1.0.0.osdk-stage-old-process");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(backup.join("payload"), b"old").unwrap();

        recover_interrupted_promotion(&final_root).unwrap();

        assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), b"old");
        assert!(transaction_debris(parent).is_empty());
    }

    #[test]
    fn first_global_install_recovery_tolerates_missing_parent_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary
            .path()
            .join("not-created/npm-global/fixture-cli/1.0.0");

        recover_interrupted_promotion(&final_root).unwrap();

        assert!(!final_root.exists());
        assert!(!final_root.parent().unwrap().exists());
    }

    #[test]
    fn legacy_interrupted_promotion_fails_closed_with_multiple_backups() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        let parent = final_root.parent().unwrap();
        std::fs::create_dir_all(parent.join(".1.0.0.osdk-backup-a")).unwrap();
        std::fs::create_dir_all(parent.join(".1.0.0.osdk-backup-b")).unwrap();

        let error = recover_interrupted_promotion(&final_root).unwrap_err();

        assert!(error.to_string().contains("multiple global npm backups"));
        assert_eq!(transaction_debris(parent).len(), 2);
    }

    #[test]
    fn successful_staged_reinstall_replaces_existing_install() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        write_valid_npm_global_install(&final_root, b"old");

        let staged = StagedInstall::begin(&final_root).unwrap();
        let stage_root = staged.root().to_path_buf();
        let staged_layout = write_valid_npm_global_install(&stage_root, b"new");
        let backend = NpmPackageBackend::from_id("npm:fixture-cli").unwrap();
        let mut version = ToolVersion::new("npm:fixture-cli", "1.0.0");
        version
            .options
            .insert(INSTALLER_OPTION.into(), NpmInstaller::Npm.as_str().into());
        for root in [&final_root, &stage_root] {
            rewrite_manifest_option_identity(root, &version.options);
        }
        assert!(completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Npm,
            "22.1.0",
            &staged_layout,
            None,
        )
        .unwrap());
        let mut promoted = staged.promote().unwrap();
        assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), b"new");
        let final_layout = GlobalInstallLayout::for_root(
            final_root.clone(),
            NpmInstaller::Npm,
            osdk_core::platform::Platform::current(),
        );
        assert!(completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Npm,
            "22.1.0",
            &final_layout,
            None,
        )
        .unwrap());
        promoted.finish();

        assert!(!stage_root.exists());
        assert!(transaction_debris(final_root.parent().unwrap()).is_empty());
    }

    #[test]
    fn promoted_install_rolls_back_to_existing_install() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        std::fs::create_dir_all(&final_root).unwrap();
        std::fs::write(final_root.join("payload"), b"old").unwrap();

        let staged = StagedInstall::begin(&final_root).unwrap();
        std::fs::write(staged.root().join("payload"), b"new").unwrap();
        let mut promoted = staged.promote().unwrap();
        promoted.rollback().unwrap();

        assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), b"old");
        assert!(transaction_debris(final_root.parent().unwrap()).is_empty());
    }

    #[test]
    fn failed_stage_promotion_restores_backup_and_cleans_transaction_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        std::fs::create_dir_all(&final_root).unwrap();
        std::fs::write(final_root.join("payload"), b"old").unwrap();
        let mut staged = StagedInstall::begin(&final_root).unwrap();
        std::fs::write(staged.root().join("payload"), b"new").unwrap();
        let mut calls = 0usize;
        let error = staged
            .promote_with(|from, to| {
                calls += 1;
                if calls == 2 {
                    Err(std::io::Error::other("injected stage rename failure"))
                } else {
                    std::fs::rename(from, to)
                }
            })
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("promoting staged global install"));
        drop(staged);

        assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), b"old");
        assert!(transaction_debris(final_root.parent().unwrap()).is_empty());
    }

    #[test]
    fn publish_failure_restores_old_install_and_publication_under_global_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/tool/1.0.0");
        let config = temporary.path().join("config.toml");
        let lock = temporary.path().join("osdk.lock");
        let shim = temporary.path().join("shims/tool");
        std::fs::create_dir_all(&final_root).unwrap();
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::fs::write(final_root.join("payload"), b"old").unwrap();
        std::fs::write(&config, b"old-config").unwrap();
        std::fs::write(&lock, b"old-lock").unwrap();
        std::fs::write(&shim, b"old-shim").unwrap();

        let staged = StagedInstall::begin(&final_root).unwrap();
        std::fs::write(staged.root().join("payload"), b"new").unwrap();
        let snapshots = PublicationSnapshot {
            paths: vec![
                (config.clone(), snapshot_path(&config).unwrap()),
                (lock.clone(), snapshot_path(&lock).unwrap()),
            ],
        };
        let dirs = test_dirs(temporary.path());
        let result = with_global_npm_state_lock(&dirs, || -> Result<()> {
            let mut promoted = staged.promote()?;
            std::fs::write(&config, b"new-config")?;
            std::fs::write(&lock, b"new-lock")?;
            std::fs::write(&shim, b"new-shim")?;
            let publish_error = anyhow!("injected publish failure");
            let snapshot_error = snapshots.restore().err();
            let replacement_error = promoted.rollback().err();
            Err(with_rollback_context(
                publish_error,
                snapshot_error,
                replacement_error,
            ))
        });

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("injected publish failure"));
        assert_eq!(std::fs::read(final_root.join("payload")).unwrap(), b"old");
        assert_eq!(std::fs::read(config).unwrap(), b"old-config");
        assert_eq!(std::fs::read(lock).unwrap(), b"old-lock");
        assert_eq!(std::fs::read(shim).unwrap(), b"new-shim");
        assert!(transaction_debris(final_root.parent().unwrap()).is_empty());
    }

    #[test]
    fn publication_snapshot_covers_old_and_new_bin_union() {
        let old = ["old-command".to_string()];
        let new = ["new-command".to_string()];
        let affected = old.iter().chain(&new).cloned().collect::<BTreeSet<_>>();
        assert_eq!(
            affected.into_iter().collect::<Vec<_>>(),
            vec!["new-command".to_string(), "old-command".to_string()]
        );
        assert!(old.iter().any(|name| !new.contains(name)));
    }

    #[test]
    fn corrupt_old_manifest_does_not_block_staged_repair() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("old-global");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            osdk_core::inventory::DynamicToolManifest::manifest_path(&root),
            b"not-json",
        )
        .unwrap();
        assert!(manifest_bin_names_best_effort(&root).is_empty());
    }

    #[test]
    fn version_switch_reads_old_bins_from_fresh_global_config_and_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = test_dirs(temporary.path());
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::create_dir_all(&dirs.data).unwrap();
        std::fs::write(
            dirs.user_config_file(),
            "[tools]\n\"npm:fixture-cli\" = \"latest\"\n",
        )
        .unwrap();
        let mut stale_config =
            osdk_core::config::Config::load_user(&dirs.user_config_file()).unwrap();
        stale_config.global_tool_configs.clear();
        let ctx = osdk_core::backend::Ctx {
            dirs: dirs.clone(),
            platform: osdk_core::platform::Platform::current(),
            config: stale_config,
            client: osdk_core::http::client().unwrap(),
            cas: std::sync::Arc::new(osdk_core::store::Cas::new(dirs.store.clone())),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:fixture-cli").unwrap();
        let mut old_version = ToolVersion::new(backend.id(), "1.0.0");
        old_version
            .options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), "22.1.0".into());
        let old_root = backend.global_install_root_for(&ctx, &old_version).unwrap();
        write_manifest_with_bins(&old_root, "1.0.0", &["old-command"]);
        let mut incoming_version = ToolVersion::new(backend.id(), "2.0.0");
        incoming_version
            .options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), "22.1.0".into());
        let incoming_root = backend
            .global_install_root_for(&ctx, &incoming_version)
            .unwrap();
        write_manifest_with_bins(&incoming_root, "2.0.0", &["new-command"]);
        let mut locked_version = ToolVersion::new(backend.id(), "1.0.0");
        locked_version
            .options
            .insert(LOCKED_NPM_INSTALLER_OPTION.into(), "npm".into());
        locked_version
            .options
            .insert(LOCKED_NPM_SCOPE_OPTION.into(), "global".into());
        locked_version
            .options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), "22.1.0".into());
        crate::lockfile::upsert_resolved_with_scope(
            &dirs.user_lock_file(),
            ctx.platform,
            &dirs,
            &ToolRequest::parse("npm:fixture-cli@latest").unwrap(),
            &locked_version,
            crate::lockfile::LockScope::Global,
        )
        .unwrap();

        assert_eq!(
            selected_global_bin_names(&ctx, &backend, &incoming_root).unwrap(),
            vec!["new-command".to_string(), "old-command".to_string()]
        );
    }

    #[test]
    fn global_state_lock_serializes_snapshot_and_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = test_dirs(temporary.path());
        let value = temporary.path().join("state");
        std::fs::write(&value, b"initial").unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_dirs = dirs.clone();
        let first_value = value.clone();
        let first = std::thread::spawn(move || {
            with_global_npm_state_lock(&first_dirs, || {
                std::fs::write(&first_value, b"first")?;
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        entered_rx.recv().unwrap();
        let second_dirs = dirs.clone();
        let second_value = value.clone();
        let second = std::thread::spawn(move || {
            with_global_npm_state_lock(&second_dirs, || {
                let observed = std::fs::read(&second_value)?;
                std::fs::write(&second_value, [observed, b"+second".to_vec()].concat())?;
                Ok(())
            })
            .unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(std::fs::read(&value).unwrap(), b"first");
        release_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(std::fs::read(value).unwrap(), b"first+second");
    }

    #[cfg(unix)]
    #[test]
    fn normalized_launcher_survives_staging_promotion() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let final_root = temporary.path().join("npm-global/prettier/3.6.2");
        let staged = StagedInstall::begin(&final_root).unwrap();
        let layout = GlobalInstallLayout::for_root(
            staged.root().to_path_buf(),
            NpmInstaller::Npm,
            osdk_core::platform::Platform::current(),
        );
        let package = layout.root.join("lib/node_modules/prettier");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(&layout.bin).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"prettier","version":"3.6.2","bin":{"prettier":"cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(package.join("cli.js"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            package.join("cli.js"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink(package.join("cli.js"), layout.bin.join("prettier")).unwrap();
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        normalize_global_bins(&backend, &version, NpmInstaller::Npm, &layout).unwrap();
        let target = std::fs::read_link(layout.bin.join("prettier")).unwrap();
        assert!(!target.is_absolute());

        let mut promoted = staged.promote().unwrap();
        let launcher = final_root.join("bin/prettier");
        let resolved = dunce::canonicalize(&launcher).unwrap();
        assert!(resolved.starts_with(dunce::canonicalize(&final_root).unwrap()));
        assert!(!resolved.to_string_lossy().contains(".osdk-stage-"));
        promoted.finish();
    }

    fn transaction_debris(parent: &Path) -> Vec<OsString> {
        std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .filter(|name| {
                let name = name.to_string_lossy();
                name.contains(".osdk-stage-")
                    || name.contains(".osdk-backup-")
                    || name.contains(".osdk-failed-")
            })
            .collect()
    }

    fn test_dirs(root: &Path) -> osdk_core::dirs::Dirs {
        osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap()
    }

    fn write_valid_npm_global_install(root: &Path, payload: &[u8]) -> GlobalInstallLayout {
        let layout = GlobalInstallLayout::for_root(
            root.to_path_buf(),
            NpmInstaller::Npm,
            osdk_core::platform::Platform::current(),
        );
        let package = if cfg!(windows) {
            root.join("node_modules/fixture-cli")
        } else {
            root.join("lib/node_modules/fixture-cli")
        };
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(&layout.bin).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"fixture-cli","version":"1.0.0","bin":{"fixture-cli":"cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(package.join("cli.js"), "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            path_relative_to(
                &dunce::canonicalize(package.join("cli.js")).unwrap(),
                &layout.bin,
            )
            .unwrap(),
            layout.bin.join("fixture-cli"),
        )
        .unwrap();
        #[cfg(windows)]
        {
            let relative = path_relative_to(
                &dunce::canonicalize(package.join("cli.js")).unwrap(),
                &layout.bin,
            )
            .unwrap();
            std::fs::write(
                layout.bin.join("fixture-cli.cmd"),
                render_windows_node_wrapper(&relative).unwrap(),
            )
            .unwrap();
        }
        let options = BTreeMap::from([(INSTALLER_OPTION.into(), "npm".into())]);
        let identity = osdk_core::tool::InstallIdentity::new(
            "npm:fixture-cli",
            "1.0.0",
            osdk_core::platform::Platform::current().to_string(),
            osdk_core::tool::InstallScope::Global,
            &options,
            vec![osdk_core::tool::InstallDependency {
                kind: osdk_core::tool::InstallDependencyKind::Runtime,
                id: "node".into(),
                version: "22.1.0".into(),
                identity: None,
            }],
            BTreeMap::new(),
        )
        .unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: "fixture-cli".into(),
            path: if cfg!(windows) {
                "fixture-cli.cmd".into()
            } else {
                "bin/fixture-cli".into()
            },
        }];
        manifest.write_atomic(root).unwrap();
        std::fs::write(
            root.join(".osdk-npm-receipt.json"),
            r#"{"schema":1,"provider":"npm-package","package":"fixture-cli","installer":"npm","node_version":"22.1.0","build_policy":"deny"}"#,
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        std::fs::write(root.join("payload"), payload).unwrap();
        layout
    }

    fn rewrite_manifest_option_identity(root: &Path, options: &BTreeMap<String, String>) {
        let old = osdk_core::inventory::DynamicToolManifest::load(root).unwrap();
        let identity = osdk_core::tool::InstallIdentity::new(
            &old.identity.tool,
            &old.identity.version,
            &old.identity.platform,
            old.identity.scope,
            options,
            old.identity.dependencies,
            old.identity.materials,
        )
        .unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = old.bins;
        manifest.write_atomic(root).unwrap();
    }

    fn write_manifest_with_bins(root: &Path, version: &str, names: &[&str]) {
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let identity = osdk_core::tool::InstallIdentity::new(
            "npm:fixture-cli",
            version,
            osdk_core::platform::Platform::current().to_string(),
            osdk_core::tool::InstallScope::Global,
            &BTreeMap::new(),
            Vec::new(),
            BTreeMap::new(),
        )
        .unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = names
            .iter()
            .map(|name| {
                let path = format!("bin/{name}");
                std::fs::write(root.join(&path), b"fixture").unwrap();
                osdk_core::inventory::DynamicToolBin {
                    name: (*name).into(),
                    path,
                }
            })
            .collect();
        manifest.write_atomic(root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
    }

    #[test]
    fn native_arguments_use_real_global_modes_with_controlled_roots() {
        let install_root = Path::new("/tmp/osdk-global/install");
        let bin_dir = install_root.join("bin");
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some("/tmp/osdk-global/data".into()),
            "OSDK_CACHE_DIR" => Some("/tmp/osdk-global/cache".into()),
            "OSDK_CONFIG_DIR" => Some("/tmp/osdk-global/config".into()),
            _ => None,
        })
        .unwrap();
        let npm = native_args(
            NpmInstaller::Npm,
            install_root,
            &bin_dir,
            "prettier@3.6.2",
            &version,
            &dirs,
            false,
        )
        .unwrap();
        assert_eq!(npm.first().map(String::as_str), Some("install"));
        assert!(npm.iter().any(|arg| arg == "--global"));
        assert!(npm
            .windows(2)
            .any(|pair| pair[0] == "--prefix" && pair[1] == install_root.display().to_string()));
        assert!(npm.iter().any(|arg| arg == "--ignore-scripts"));

        let pnpm = native_args(
            NpmInstaller::Pnpm,
            install_root,
            &bin_dir,
            "prettier@3.6.2",
            &version,
            &dirs,
            false,
        )
        .unwrap();
        assert_eq!(pnpm.first().map(String::as_str), Some("add"));
        assert!(pnpm.iter().any(|arg| arg == "--global"));
        assert!(pnpm
            .windows(2)
            .any(|pair| pair[0] == "--global-bin-dir" && pair[1] == bin_dir.display().to_string()));
        assert!(pnpm.windows(2).any(|pair| pair[0] == "--store-dir"
            && pair[1] == dirs.store.join("pnpm-store").display().to_string()));

        let offline = native_args(
            NpmInstaller::Npm,
            install_root,
            &bin_dir,
            "prettier@3.6.2",
            &version,
            &dirs,
            true,
        )
        .unwrap();
        assert!(offline.iter().any(|arg| arg == "--offline"));
        assert_eq!(
            native_preflight_args(NpmInstaller::Npm, "prettier@3.6.2", true),
            vec!["install", "prettier@3.6.2", "--offline"]
        );
    }

    #[test]
    fn npm_global_completion_does_not_require_a_native_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temporary.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temporary.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temporary.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let ctx = osdk_core::backend::Ctx {
            dirs: dirs.clone(),
            platform: osdk_core::platform::Platform::current(),
            config: osdk_core::config::Config::load_user(&dirs.user_config_file()).unwrap(),
            client: osdk_core::http::client().unwrap(),
            cas: std::sync::Arc::new(osdk_core::store::Cas::new(dirs.store.clone())),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let mut version = ToolVersion::new("npm:prettier", "3.6.2");
        version
            .options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), "22.1.0".into());
        let root = backend.global_install_root_for(&ctx, &version).unwrap();
        let layout = GlobalInstallLayout::for_root(root.clone(), NpmInstaller::Npm, ctx.platform);
        std::fs::create_dir_all(&layout.bin).unwrap();
        let package = if cfg!(windows) {
            root.join("node_modules/prettier")
        } else {
            root.join("lib/node_modules/prettier")
        };
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"prettier","version":"3.6.2","bin":{"prettier":"bin.js"}}"#,
        )
        .unwrap();
        std::fs::write(package.join("bin.js"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            Path::new("../lib/node_modules/prettier/bin.js"),
            layout.bin.join("prettier"),
        )
        .unwrap();
        #[cfg(windows)]
        std::fs::write(
            layout.bin.join("prettier.cmd"),
            "@echo off\r\nnode \"%~dp0node_modules\\prettier\\bin.js\" %*\r\n",
        )
        .unwrap();
        let mut version = version;
        version
            .options
            .insert(INSTALLER_OPTION.into(), NpmInstaller::Npm.as_str().into());
        let identity = backend
            .install_identity(&ctx, &version, ToolScope::Global)
            .unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: "prettier".into(),
            path: if cfg!(windows) {
                "prettier.cmd".into()
            } else {
                "bin/prettier".into()
            },
        }];
        manifest.write_atomic(&root).unwrap();
        std::fs::write(
            root.join(".osdk-npm-receipt.json"),
            r#"{"schema":1,"provider":"npm-package","package":"prettier","installer":"npm","node_version":"22.1.0","build_policy":"deny"}"#,
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();

        assert!(completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Npm,
            "22.1.0",
            &layout,
            None,
        )
        .unwrap());
    }

    #[test]
    fn completed_global_install_rejects_different_option_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("npm-global/fixture-cli/1.0.0");
        let layout = write_valid_npm_global_install(&root, b"installed");
        let backend = NpmPackageBackend::from_id("npm:fixture-cli").unwrap();
        let mut version = ToolVersion::new(backend.id(), "1.0.0");
        version
            .options
            .insert(INSTALLER_OPTION.into(), NpmInstaller::Npm.as_str().into());
        rewrite_manifest_option_identity(&root, &version.options);

        assert!(completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Npm,
            "22.1.0",
            &layout,
            None,
        )
        .unwrap());

        version.options.insert("allow_builds".into(), "true".into());
        assert!(!completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Npm,
            "22.1.0",
            &layout,
            None,
        )
        .unwrap());
    }

    #[test]
    fn completed_global_install_requires_matching_native_digest_and_runtime() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = osdk_core::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temporary.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temporary.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temporary.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let ctx = osdk_core::backend::Ctx {
            dirs: dirs.clone(),
            platform: osdk_core::platform::Platform::current(),
            config: osdk_core::config::Config::load_user(&dirs.user_config_file()).unwrap(),
            client: osdk_core::http::client().unwrap(),
            cas: std::sync::Arc::new(osdk_core::store::Cas::new(dirs.store.clone())),
            show_progress: false,
        };
        let backend = NpmPackageBackend::from_id("npm:prettier").unwrap();
        let mut version = ToolVersion::new("npm:prettier", "3.6.2");
        version
            .options
            .insert(LOCKED_NPM_NODE_VERSION_OPTION.into(), "22.1.0".into());
        let root = backend.global_install_root_for(&ctx, &version).unwrap();
        let layout = GlobalInstallLayout::for_root(root.clone(), NpmInstaller::Aube, ctx.platform);
        let project = layout.project.clone();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::fs::write(
            project.join("package.json"),
            r#"{"name":"aube-global","version":"0.0.0","private":true,"dependencies":{"prettier":"3.6.2"}}"#,
        )
        .unwrap();
        let package = project.join("node_modules/prettier");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"prettier","version":"3.6.2","bin":{"prettier":"bin.js"}}"#,
        )
        .unwrap();
        std::fs::write(package.join("bin.js"), "").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                Path::new("../prettier/bin.js"),
                project.join("node_modules/.bin/prettier"),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                Path::new("../project/node_modules/.bin/prettier"),
                layout.bin.join("prettier"),
            )
            .unwrap();
        }
        #[cfg(windows)]
        {
            std::fs::write(
                project.join("node_modules/.bin/prettier.cmd"),
                "@echo off\r\nnode \"%~dp0..\\prettier\\bin.js\" %*\r\n",
            )
            .unwrap();
            std::fs::write(
                layout.bin.join("prettier.cmd"),
                "@echo off\r\nnode \"%~dp0..\\project\\node_modules\\prettier\\bin.js\" %*\r\n",
            )
            .unwrap();
        }
        std::fs::write(
            project.join("aube-lock.yaml"),
            "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      prettier:\n        specifier: 3.6.2\n        version: 3.6.2\npackages:\n  prettier@3.6.2:\n    resolution: {integrity: sha512-Zml4dHVyZQ==}\n",
        )
        .unwrap();
        let digest = read_native_lock(&project.join("aube-lock.yaml"), NpmInstaller::Aube).unwrap();
        let mut version = version;
        version
            .options
            .insert(INSTALLER_OPTION.into(), NpmInstaller::Aube.as_str().into());
        let identity = backend
            .install_identity(&ctx, &version, ToolScope::Global)
            .unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::from_identity(identity).unwrap();
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: "prettier".into(),
            path: if cfg!(windows) {
                "bin/prettier.cmd".into()
            } else {
                "bin/prettier".into()
            },
        }];
        manifest.write_atomic(&root).unwrap();
        std::fs::write(
            root.join(".osdk-npm-receipt.json"),
            serde_json::to_vec(&osdk_core::backend::npm_package::NpmInstallReceipt {
                schema: 1,
                provider: "npm-package".into(),
                package: "prettier".into(),
                installer: "aube".into(),
                node_version: "22.1.0".into(),
                build_policy: "deny".into(),
                graph_sha256: None,
                root_integrity: None,
                root_source: None,
                native_lock_format: Some("aube-v9".into()),
                native_lock_sha256: Some(digest.sha256),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        assert!(completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Aube,
            "22.1.0",
            &layout,
            Some(&project.join("aube-lock.yaml"))
        )
        .unwrap());
        std::fs::write(
            project.join("aube-lock.yaml"),
            "lockfileVersion: '9.0'\nchanged: true\n",
        )
        .unwrap();
        assert!(!completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Aube,
            "22.1.0",
            &layout,
            Some(&project.join("aube-lock.yaml"))
        )
        .unwrap());
    }
}
