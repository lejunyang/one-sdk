//! Global `npm:<package>` installation for `osdk use --global`.
//!
//! Global means user-selected and shim-visible in osdk. Native npm and pnpm
//! execute their real global-add modes against an osdk-owned prefix; embedded
//! Aube uses its safe synthetic-project equivalent. Neither path mutates the
//! caller's project or an ambient Node installation.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use osdk_core::backend::aube_host::{self, EmbeddedInstallRequest};
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
/// power-loss transaction. The journal makes the next operation deterministic:
/// an activated publication is kept, while every earlier phase is rolled back.
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

    if journal.phase == PromotionPhase::Activated {
        if !final_root.exists() {
            return Err(anyhow!(
                "activated global npm promotion is missing canonical install {}",
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
    let name = final_root
        .file_name()
        .ok_or_else(|| anyhow!("global install root has no file name"))?
        .to_string_lossy();
    let prefix = format!(".{name}.osdk-{kind}-");
    let mut paths = std::fs::read_dir(parent)?
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
    let name = final_root
        .file_name()
        .ok_or_else(|| anyhow!("global install root has no file name"))?
        .to_string_lossy();
    let prefixes = [
        format!(".{name}.osdk-stage-"),
        format!(".{name}.osdk-backup-"),
        format!(".{name}.osdk-failed-"),
    ];
    for entry in std::fs::read_dir(parent)? {
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
    apply_source_override(app, &request.backend);
    let requested_installer = npm_tools::installer_from_request_options(&request.options)?;
    let cwd = std::env::current_dir().context("getting current dir for global npm install")?;
    let plan = npm_tools::plan_npm_installer(&cwd, requested_installer, ToolScope::Global)?;
    let runtime = ensure_managed_runtime(app, plan.installer).await?;

    request.options.insert(
        LOCKED_NPM_INSTALLER_OPTION.into(),
        plan.installer.as_str().into(),
    );
    request.options.insert(
        LOCKED_NPM_SCOPE_OPTION.into(),
        ToolScope::Global.as_str().into(),
    );
    request.options.insert(
        LOCKED_NPM_NODE_VERSION_OPTION.into(),
        runtime.node_version.version.clone(),
    );

    let backend = NpmPackageBackend::from_id(&request.backend)
        .ok_or_else(|| anyhow!("invalid npm package backend `{}`", request.backend))?;
    if app.refresh_sources {
        select::refresh(&app.ctx, &backend).await?;
    }
    let effective = expand_alias(app, &request)?;
    let mut version = backend
        .resolve_version(&app.ctx, &effective)
        .await
        .with_context(|| format!("resolving {}@{}", request.backend, request.spec))?;
    version.options = request.options.clone();
    version
        .options
        .insert(INSTALLER_OPTION.into(), plan.installer.as_str().into());

    let final_layout = GlobalInstallLayout::for_root(
        backend.global_install_root(&app.ctx, &version.version),
        plan.installer,
        app.ctx.platform,
    );
    let lock_path = app.ctx.dirs.lock_dir(backend.id()).join(format!(
        "{}.global.lock",
        osdk_core::dirs::sanitize_version_component(&version.version)
    ));
    let _mutation_lock = osdk_core::lock::FileLock::acquire(lock_path)?;
    recover_interrupted_promotion(&final_layout.root)?;

    let installed_native_lock = native_lock_path(&final_layout, plan.installer);
    let mut staged_install = None;
    if !completed_install_matches_at(
        &backend,
        &version,
        plan.installer,
        &runtime.node_version.version,
        &final_layout,
        installed_native_lock.as_deref(),
    )? {
        let staged = StagedInstall::begin(&final_layout.root)?;
        let staged_layout = GlobalInstallLayout::for_root(
            staged.root().to_path_buf(),
            plan.installer,
            app.ctx.platform,
        );
        run_global_install(
            app,
            &backend,
            &version,
            plan.installer,
            &runtime,
            &staged_layout,
        )
        .await?;
        normalize_global_bins(&backend, &version, plan.installer, &staged_layout)?;
        validate_global_package_identity(&backend, &version, plan.installer, &staged_layout)?;
        let staged_native_lock = native_lock_path(&staged_layout, plan.installer);
        let native = staged_native_lock
            .as_deref()
            .filter(|path| path.is_file())
            .map(|path| read_native_lock(path, plan.installer))
            .transpose()?;
        inject_native_metadata(
            &mut version,
            plan.installer,
            &runtime.node_version.version,
            native.as_ref(),
        );
        backend
            .finalize_global_install_at(
                &app.ctx,
                &version,
                &staged_layout.root,
                &staged_layout.bin,
                &runtime.node_version.version,
                plan.installer.as_str(),
                native
                    .as_ref()
                    .map(|native| (native.format.as_str(), native.sha256.as_str())),
            )
            .map_err(anyhow::Error::new)?;
        if !completed_install_matches_at(
            &backend,
            &version,
            plan.installer,
            &runtime.node_version.version,
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
            &mut version,
            plan.installer,
            &runtime.node_version.version,
            native.as_ref(),
        );
    }

    let persisted_spec = requested_spec.unwrap_or_else(|| version.version.clone());
    with_global_npm_state_lock(&app.ctx.dirs, || {
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
        // Arm crash recovery before changing the active config. From this
        // durable point, an abrupt exit keeps the fully validated new root.
        // A normal error still rolls every publication step back below.
        let publish_result = (|| {
            crate::commands::generate_shims_for(app, &backend, &version)?;
            if let Some(replacement) = replacement.as_ref() {
                replacement.mark_activated()?;
            }
            persist_global_config(app, &request, &persisted_spec, plan.installer)?;
            persist_global_lock(app, &request, &version, &runtime)?;
            let refreshed = refreshed_app(app)?;
            crate::commands::remove_stale_shims_without_other_owners(
                &refreshed,
                backend.id(),
                &old_bin_names,
                &new_bin_names,
            )
        })();
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
) -> Result<()> {
    let project = &layout.project;
    match installer {
        NpmInstaller::Aube => {
            write_aube_project_manifest(project, backend.package(), &version.version, version)?;
            let source = selected_package_source(app, backend).await?;
            let package_spec = format!("{}@{}", backend.package(), version.version);
            let (scripts_enabled, allow_all) = build_policy_flags(version)?;
            aube_host::install_global_package(EmbeddedInstallRequest {
                project_dir: project,
                packages: std::slice::from_ref(&package_spec),
                cache_dir: NpmPackageBackend::aube_cache_dir(&app.ctx),
                store_dir: NpmPackageBackend::aube_store_dir(&app.ctx),
                node_bin_dir: runtime.node_bin.clone(),
                scripts_enabled,
                dangerously_allow_all_builds: allow_all,
                offline: app.ctx.config.settings.offline,
                registry: source,
            })
            .await
            .map_err(anyhow::Error::new)?;
            Ok(())
        }
        NpmInstaller::Npm | NpmInstaller::Pnpm => {
            run_native_installer(app, backend, version, installer, runtime, layout).await
        }
        NpmInstaller::Auto => unreachable!("global planning always produces a concrete installer"),
    }
}

fn write_aube_project_manifest(
    project: &Path,
    package: &str,
    version: &str,
    tool: &ToolVersion,
) -> Result<()> {
    std::fs::create_dir_all(project)?;
    let mut manifest = serde_json::json!({
        "name": "osdk-global-npm-tool",
        "private": true
    });
    if let Some(raw) = tool.options.get("allow_builds") {
        let lower = raw.trim().to_ascii_lowercase();
        if !matches!(
            lower.as_str(),
            "" | "false" | "0" | "no" | "off" | "true" | "1" | "yes" | "on"
        ) {
            let allow_builds = raw
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| (item.to_ascii_lowercase(), serde_json::Value::Bool(true)))
                .collect::<serde_json::Map<_, _>>();
            manifest["aube"] = serde_json::json!({ "allowBuilds": allow_builds });
        }
    }
    // Aube's embedded add writes the exact dependency into this private
    // manifest; retaining the inputs here makes the staging intent explicit.
    manifest["osdkRequestedPackage"] = serde_json::Value::String(package.into());
    manifest["osdkRequestedVersion"] = serde_json::Value::String(version.into());
    std::fs::write(
        project.join("package.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
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

async fn selected_package_source(app: &App, backend: &dyn Backend) -> Result<Option<String>> {
    let sources = select::ranked_source_list(&app.ctx, backend).await?;
    Ok(sources.first().map(|source| source.download_url.clone()))
}

async fn run_native_installer(
    app: &App,
    backend: &NpmPackageBackend,
    version: &ToolVersion,
    installer: NpmInstaller,
    runtime: &ManagedRuntime,
    layout: &GlobalInstallLayout,
) -> Result<()> {
    let (_, manager_version, executable) = runtime
        .manager
        .as_ref()
        .ok_or_else(|| anyhow!("managed {installer} was not prepared"))?;
    let (manager, executable_alias) = match installer {
        NpmInstaller::Npm => (PackageManager::Npm, "npm"),
        NpmInstaller::Pnpm => (PackageManager::Pnpm, "pnpm"),
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
    let logical_args =
        native_preflight_args(installer, &package_spec, app.ctx.config.settings.offline);
    let registry_plan = package_registry::plan(
        &app.ctx,
        &layout.root,
        manager,
        executable_alias,
        &logical_args,
        |_| None,
    )
    .await?;
    let mut env = isolated_native_env(
        app,
        &layout.root,
        &layout.bin,
        installer,
        &runtime.node_bin,
        manager_version,
    )?;
    match registry_plan {
        RegistryPlan::Selected { url, .. } => {
            env.insert(package_registry::registry_env(manager).into(), url);
        }
        RegistryPlan::Unavailable { probes } => {
            return Err(unavailable_registry_error(manager, &probes));
        }
        RegistryPlan::PassThrough { .. } if app.ctx.config.settings.offline => {}
        RegistryPlan::PassThrough { reason } => {
            return Err(anyhow!(
                "cannot safely isolate global {manager} install: registry preflight passed through ({reason})"
            ));
        }
    }
    run_managed_command(executable, &args, &env, &layout.root)
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
    let allow_builds = version.options.get("allow_builds").map(String::as_str);
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
            match allow_builds {
                None | Some("" | "false" | "0" | "no" | "off") => {
                    args.push("--ignore-scripts".into())
                }
                Some("true" | "1" | "yes" | "on") => {}
                Some(_) => {
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
            match allow_builds {
                None | Some("" | "false" | "0" | "no" | "off") => {
                    args.push("--ignore-scripts".into())
                }
                Some("true" | "1" | "yes" | "on") => {
                    args.push("--dangerously-allow-all-builds".into())
                }
                Some(packages) => {
                    for package in packages
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                    {
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
        Err(anyhow!(
            "managed installer {} failed with {}:\n{}",
            executable.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
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
        Some(entry) => selected_global_version(ctx, backend, entry.version())?,
        None => None,
    };
    if let Some(selected) = selected {
        let version = ToolVersion::new(backend.id(), selected);
        let root = backend.global_install_root(ctx, &version.version);
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
    configured_spec: &str,
) -> Result<Option<String>> {
    let spec = VersionSpec::parse(configured_spec);
    if let VersionSpec::Exact(version) = &spec {
        return Ok(Some(version.clone()));
    }
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
                return Ok(Some(version));
            }
        }
    }
    let installed = backend.list_installed(ctx)?;
    let candidates = installed
        .iter()
        .map(osdk_core::version::VersionInfo::stable)
        .collect::<Vec<_>>();
    Ok(osdk_core::version::select_version(&spec, &candidates)
        .map(|version| version.version.clone()))
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
            manifest.metadata.get("native_lock_format") == Some(&native.format)
                && manifest.metadata.get("lock_sha256") == Some(&native.sha256)
        }
        None => {
            !manifest.metadata.contains_key("native_lock_format")
                && !manifest.metadata.contains_key("lock_sha256")
        }
    };
    let bin_dir = &layout.bin;
    Ok(manifest.id == version.backend
        && manifest.version.as_deref() == Some(version.version.as_str())
        && manifest.metadata.get("installer").map(String::as_str) == Some(installer.as_str())
        && manifest.metadata.get("scope").map(String::as_str) == Some("global")
        && manifest.metadata.get("node_version").map(String::as_str) == Some(node_version)
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
        .map(|(key, value)| (key.clone(), option_value(key, value)))
        .collect::<BTreeMap<_, _>>();
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

fn option_value(key: &str, value: &str) -> osdk_core::config::ToolConfigValue {
    if key == "allow_builds" {
        return match value.to_ascii_lowercase().as_str() {
            "true" => osdk_core::config::ToolConfigValue::Bool(true),
            "false" => osdk_core::config::ToolConfigValue::Bool(false),
            _ => osdk_core::config::ToolConfigValue::Array(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
        };
    }
    osdk_core::config::ToolConfigValue::String(value.into())
}

fn build_policy_flags(version: &ToolVersion) -> Result<(bool, bool)> {
    let Some(raw) = version.options.get("allow_builds") else {
        return Ok((false, false));
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "false" | "0" | "no" | "off" => Ok((false, false)),
        "true" | "1" | "yes" | "on" => Ok((true, true)),
        _ => Ok((true, false)),
    }
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
        let version = ToolVersion::new("npm:fixture-cli", "1.0.0");
        let layout = write_valid_npm_global_install(&final_root, b"old");
        assert!(completed_install_matches_at(
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

            let expected = if phase == PromotionPhase::Activated {
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
    fn interrupted_first_install_without_previous_root_is_removed() {
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
                phase == PromotionPhase::Activated,
                "phase {phase:?}"
            );
            assert!(transaction_debris(parent).is_empty());
        }
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
        let version = ToolVersion::new("npm:fixture-cli", "1.0.0");
        assert!(completed_install_matches_at(
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
        let old_root = backend.global_install_root(&ctx, "1.0.0");
        write_manifest_with_bins(&old_root, "1.0.0", &["old-command"]);
        let incoming_root = backend.global_install_root(&ctx, "2.0.0");
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
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::new("npm:fixture-cli").unwrap();
        manifest.version = Some("1.0.0".into());
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: "fixture-cli".into(),
            path: if cfg!(windows) {
                "fixture-cli.cmd".into()
            } else {
                "bin/fixture-cli".into()
            },
        }];
        manifest.metadata = BTreeMap::from([
            ("installer".into(), "npm".into()),
            ("scope".into(), "global".into()),
            ("node_version".into(), "22.1.0".into()),
        ]);
        manifest.write_atomic(root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        std::fs::write(root.join("payload"), payload).unwrap();
        layout
    }

    fn write_manifest_with_bins(root: &Path, version: &str, names: &[&str]) {
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let mut manifest =
            osdk_core::inventory::DynamicToolManifest::new("npm:fixture-cli").unwrap();
        manifest.version = Some(version.into());
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
        manifest.metadata.insert("scope".into(), "global".into());
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
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        let root = backend.global_install_root(&ctx, &version.version);
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
        let mut manifest = osdk_core::inventory::DynamicToolManifest::new(backend.id()).unwrap();
        manifest.version = Some(version.version.clone());
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: "prettier".into(),
            path: if cfg!(windows) {
                "prettier.cmd".into()
            } else {
                "bin/prettier".into()
            },
        }];
        manifest.metadata = BTreeMap::from([
            ("installer".into(), "npm".into()),
            ("scope".into(), "global".into()),
            ("node_version".into(), "22.1.0".into()),
        ]);
        manifest.write_atomic(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();

        assert!(completed_install_matches_at(
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
        let version = ToolVersion::new("npm:prettier", "3.6.2");
        let root = backend.global_install_root(&ctx, &version.version);
        let layout = GlobalInstallLayout::for_root(root.clone(), NpmInstaller::Aube, ctx.platform);
        let project = layout.project.clone();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
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
        std::fs::write(project.join("aube-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        let digest = read_native_lock(&project.join("aube-lock.yaml"), NpmInstaller::Aube).unwrap();
        let mut manifest = osdk_core::inventory::DynamicToolManifest::new("npm:prettier").unwrap();
        manifest.version = Some("3.6.2".into());
        manifest.bins = vec![osdk_core::inventory::DynamicToolBin {
            name: "prettier".into(),
            path: if cfg!(windows) {
                "bin/prettier.cmd".into()
            } else {
                "bin/prettier".into()
            },
        }];
        manifest.metadata = BTreeMap::from([
            ("installer".into(), "aube".into()),
            ("scope".into(), "global".into()),
            ("node_version".into(), "22.1.0".into()),
            ("native_lock_format".into(), "aube-v9".into()),
            ("lock_sha256".into(), digest.sha256),
        ]);
        manifest.write_atomic(&root).unwrap();
        std::fs::write(root.join(".osdk-complete"), b"").unwrap();
        assert!(completed_install_matches_at(
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
