//! Bun backend: installs the official platform package published to npm. This
//! avoids the GitHub Releases API and verifies the tarball with npm SRI.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Libc, Os};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct BunBackend;

impl BunBackend {
    fn platform_package(ctx: &Ctx) -> Option<&'static str> {
        Some(
            match (ctx.platform.os, ctx.platform.arch, ctx.platform.libc) {
                (Os::Linux, Arch::X64, Libc::Musl) => "@oven/bun-linux-x64-musl",
                (Os::Linux, Arch::Arm64, Libc::Musl) => "@oven/bun-linux-aarch64-musl",
                (Os::Linux, Arch::X64, _) => "@oven/bun-linux-x64",
                (Os::Linux, Arch::Arm64, _) => "@oven/bun-linux-aarch64",
                (Os::Macos, Arch::X64, _) => "@oven/bun-darwin-x64",
                (Os::Macos, Arch::Arm64, _) => "@oven/bun-darwin-aarch64",
                (Os::Windows, Arch::X64, _) => "@oven/bun-windows-x64",
                (Os::Windows, Arch::Arm64, _) => "@oven/bun-windows-aarch64",
                _ => return None,
            },
        )
    }
}

#[async_trait]
impl Backend for BunBackend {
    fn id(&self) -> &str {
        "bun"
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::mirror("npmmirror", "https://registry.npmmirror.com/", 5)
                .with_index("https://registry.npmmirror.com/bun"),
            Source::official("npm", "https://registry.npmjs.org/")
                .with_index("https://registry.npmjs.org/bun"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let versions = crate::npm::list_versions(ctx, &sources, "bun").await?;
        Ok(versions
            .into_iter()
            .filter(|version| {
                semver::Version::parse(version)
                    .map(|version| version.pre.is_empty())
                    .unwrap_or(false)
            })
            .map(VersionInfo::stable)
            .collect())
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let package = Self::platform_package(ctx).ok_or_else(|| Error::UnsupportedPlatform {
            os: format!("{:?}", ctx.platform.os),
            arch: format!("{:?}", ctx.platform.arch),
        })?;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let dist = crate::npm::resolve_dist(ctx, &sources, package, &tv.version).await?;

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: tv.version.clone(),
            urls: dist.urls,
            file_name: format!("bun-{}.tgz", tv.version),
            kind: ArchiveKind::TarGz,
            checksum: dist.checksum,
            strip_root: true,
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
        ensure_executable(
            &ctx.dirs.install_path(self.id(), &tv.version),
            ctx.platform.os,
        );
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![ctx
            .dirs
            .install_path(self.id(), &tv.version)
            .join("bin")])
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            Ok(vec!["bun".into()])
        } else {
            Ok(discovered)
        }
    }

    fn idiomatic_files(&self) -> &[&str] {
        &[".bun-version"]
    }
}

fn ensure_executable(install_dir: &std::path::Path, os: Os) {
    if os == Os::Windows {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = install_dir.join("bin/bun");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Platform;

    #[test]
    fn maps_platform_packages() {
        let linux = CtxPlatform {
            os: Os::Linux,
            arch: Arch::X64,
            libc: Libc::Glibc,
        };
        let musl = CtxPlatform {
            libc: Libc::Musl,
            ..linux
        };
        assert_eq!(
            BunBackend::platform_package(&ctx(linux)),
            Some("@oven/bun-linux-x64")
        );
        assert_eq!(
            BunBackend::platform_package(&ctx(musl)),
            Some("@oven/bun-linux-x64-musl")
        );
    }

    type CtxPlatform = Platform;

    fn ctx(platform: Platform) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some("/tmp/osdk-bun-test/data".into()),
            "OSDK_CACHE_DIR" => Some("/tmp/osdk-bun-test/cache".into()),
            "OSDK_CONFIG_DIR" => Some("/tmp/osdk-bun-test/config".into()),
            _ => None,
        })
        .unwrap();
        Ctx {
            dirs: dirs.clone(),
            platform,
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store)),
            show_progress: false,
        }
    }
}
