//! Global `npm:<package>` installation for `osdk use --global`.
//!
//! Global means user-selected and shim-visible in osdk. npm and pnpm execute
//! their real global-add modes against an osdk-owned prefix. No path mutates
//! the caller's project or an ambient Node installation.

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
pub(crate) struct GlobalInstallLayout {
    root: PathBuf,
    bin: PathBuf,
}

impl GlobalInstallLayout {
    fn for_root(
        root: PathBuf,
        installer: NpmInstaller,
        platform: osdk_core::platform::Platform,
    ) -> Self {
        let bin =
            if installer == NpmInstaller::Npm && platform.os == osdk_core::platform::Os::Windows {
                root.clone()
            } else {
                root.join("bin")
            };
        Self { root, bin }
    }
}

pub(crate) struct StagedInstall {
    final_root: PathBuf,
    stage_root: PathBuf,
    backup_root: PathBuf,
    journal_path: PathBuf,
    promoted: bool,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PromotionPhase {
    Prepared,
    OldMoved,
    NewPromoted,
    Activated,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct PromotionJournal {
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
pub(crate) struct PromotedInstall {
    final_root: PathBuf,
    backup_root: PathBuf,
    had_previous: bool,
    stage_root: PathBuf,
    journal_path: PathBuf,
    completed: bool,
}

#[derive(Clone)]
pub(crate) enum PathSnapshot {
    Absent,
    File {
        bytes: Vec<u8>,
        permissions: std::fs::Permissions,
    },
    Symlink(PathBuf),
}

pub(crate) struct PublicationSnapshot {
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

#[derive(Debug, Clone)]
pub(crate) struct ManagedRuntime {
    node_request: ToolRequest,
    node_version: ToolVersion,
    node_bin: PathBuf,
    manager: Option<(ToolRequest, ToolVersion, PathBuf)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NpmBuildPolicy {
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

#[derive(Debug)]
pub(crate) struct NativeLockIdentity {
    kind: &'static str,
    format: String,
    sha256: String,
}

mod install;
mod promotion;
pub use install::*;
pub(crate) use promotion::*;

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
            ..Default::default()
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
                    ..Default::default()
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
            ..Default::default()
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
        // pnpm is the installer that records a native lock digest, so it is the
        // one that exercises digest matching. Its real global layout lives under
        // `pnpm-global/`, which is where the lock and manifest lookups search.
        let layout = GlobalInstallLayout::for_root(root.clone(), NpmInstaller::Pnpm, ctx.platform);
        let project = root.join("pnpm-global");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(project.join("node_modules/.bin")).unwrap();
        std::fs::write(
            project.join("package.json"),
            r#"{"name":"osdk-global","version":"0.0.0","private":true,"dependencies":{"prettier":"3.6.2"}}"#,
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
                Path::new("../pnpm-global/node_modules/.bin/prettier"),
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
                "@echo off\r\nnode \"%~dp0..\\pnpm-global\\node_modules\\prettier\\bin.js\" %*\r\n",
            )
            .unwrap();
        }
        std::fs::write(
            project.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      prettier:\n        specifier: 3.6.2\n        version: 3.6.2\npackages:\n  prettier@3.6.2:\n    resolution: {integrity: sha512-Zml4dHVyZQ==}\n",
        )
        .unwrap();
        let digest = read_native_lock(&project.join("pnpm-lock.yaml"), NpmInstaller::Pnpm).unwrap();
        version
            .options
            .insert(INSTALLER_OPTION.into(), NpmInstaller::Pnpm.as_str().into());
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
            ..Default::default()
        }];
        manifest.write_atomic(&root).unwrap();
        std::fs::write(
            root.join(".osdk-npm-receipt.json"),
            serde_json::to_vec(&osdk_core::backend::npm_package::NpmInstallReceipt {
                schema: 1,
                provider: "npm-package".into(),
                package: "prettier".into(),
                installer: "pnpm".into(),
                node_version: "22.1.0".into(),
                build_policy: "deny".into(),
                graph_sha256: None,
                root_integrity: None,
                root_source: None,
                native_lock_format: Some("pnpm-v9".into()),
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
            NpmInstaller::Pnpm,
            "22.1.0",
            &layout,
            Some(&project.join("pnpm-lock.yaml"))
        )
        .unwrap());
        std::fs::write(
            project.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\nchanged: true\n",
        )
        .unwrap();
        assert!(!completed_install_matches_at(
            osdk_core::platform::Platform::current(),
            &backend,
            &version,
            NpmInstaller::Pnpm,
            "22.1.0",
            &layout,
            Some(&project.join("pnpm-lock.yaml"))
        )
        .unwrap());
    }
}
