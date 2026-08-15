//! pnpm backend: installs the standalone pnpm binary from the npm registry's
//! platform package `@pnpm/<os>-<arch>` (the same artifact `@pnpm/exe` uses).
//! This keeps the "runs without a managed Node" property while gaining
//! first-party integrity verification (npm SRI), and stays mirror-friendly.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Os};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct PnpmBackend;

impl PnpmBackend {
    /// The `@pnpm/<os>-<arch>` platform package that ships the standalone binary.
    fn platform_package(ctx: &Ctx) -> Option<&'static str> {
        Some(match (ctx.platform.os, ctx.platform.arch) {
            (Os::Linux, Arch::X64) => "@pnpm/linux-x64",
            (Os::Linux, Arch::Arm64) => "@pnpm/linux-arm64",
            (Os::Macos, Arch::X64) => "@pnpm/macos-x64",
            (Os::Macos, Arch::Arm64) => "@pnpm/macos-arm64",
            (Os::Windows, Arch::X64) => "@pnpm/win-x64",
            (Os::Windows, Arch::Arm64) => "@pnpm/win-arm64",
            _ => return None,
        })
    }
}

#[async_trait]
impl Backend for PnpmBackend {
    fn id(&self) -> &str {
        "pnpm"
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::mirror("npmmirror", "https://registry.npmmirror.com/", 5)
                .with_index("https://registry.npmmirror.com/pnpm"),
            Source::official("npm", "https://registry.npmjs.org/")
                .with_index("https://registry.npmjs.org/pnpm"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let versions = crate::npm::list_versions(ctx, &sources, "pnpm").await?;
        Ok(versions
            .into_iter()
            .map(|v| VersionInfo {
                version: v,
                stable: true,
                lts: None,
            })
            .collect())
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let pkg = Self::platform_package(ctx).ok_or_else(|| Error::UnsupportedPlatform {
            os: format!("{:?}", ctx.platform.os),
            arch: format!("{:?}", ctx.platform.arch),
        })?;
        // Resolve the platform package's tarball + SRI integrity from the registry.
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let dist = crate::npm::resolve_dist(ctx, &sources, pkg, &tv.version).await?;

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: tv.version.clone(),
            urls: dist.urls,
            file_name: format!("pnpm-{}.tgz", tv.version),
            kind: ArchiveKind::TarGz,
            checksum: dist.checksum, // npm SRI (sha512/sha256)
            strip_root: true,        // npm tarballs wrap files in package/
        };
        let pctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
            offline: ctx.config.settings.offline,
        };
        pipeline::run(&plan, &pctx).await?;
        // The tarball ships `package/pnpm` -> after strip, `pnpm` at install root.
        ensure_executable(
            &ctx.dirs.install_path(self.id(), &tv.version),
            ctx.platform.os,
        );
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        // The `pnpm` binary sits at the install root (npm `package/` stripped).
        Ok(vec![ctx.dirs.install_path(self.id(), &tv.version)])
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            Ok(vec!["pnpm".into()])
        } else {
            Ok(discovered)
        }
    }
}

fn ensure_executable(install_dir: &std::path::Path, os: Os) {
    if os == Os::Windows {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = install_dir.join("pnpm");
        if let Ok(meta) = std::fs::metadata(&bin) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = std::fs::set_permissions(&bin, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = install_dir;
    }
}
