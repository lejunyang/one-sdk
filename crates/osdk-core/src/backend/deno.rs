//! Deno backend: installs the official platform package published to npm. This
//! provides a complete version index without the GitHub Releases API and
//! verifies each tarball with npm SRI.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Libc, Os};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct DenoBackend;

impl DenoBackend {
    fn platform_package(ctx: &Ctx) -> Option<&'static str> {
        Some(
            match (ctx.platform.os, ctx.platform.arch, ctx.platform.libc) {
                (Os::Linux, Arch::X64, Libc::Glibc) => "@deno/linux-x64-glibc",
                (Os::Linux, Arch::Arm64, Libc::Glibc) => "@deno/linux-arm64-glibc",
                (Os::Macos, Arch::X64, _) => "@deno/darwin-x64",
                (Os::Macos, Arch::Arm64, _) => "@deno/darwin-arm64",
                (Os::Windows, Arch::X64, _) => "@deno/win32-x64",
                (Os::Windows, Arch::Arm64, _) => "@deno/win32-arm64",
                _ => return None,
            },
        )
    }
}

#[async_trait]
impl Backend for DenoBackend {
    fn id(&self) -> &str {
        "deno"
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::mirror("npmmirror", "https://registry.npmmirror.com/", 5)
                .with_index("https://registry.npmmirror.com/deno"),
            Source::official("npm", "https://registry.npmjs.org/")
                .with_index("https://registry.npmjs.org/deno"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let versions = crate::npm::list_versions(ctx, &sources, "deno").await?;
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
        let plan = if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
            plan
        } else {
            let package =
                Self::platform_package(ctx).ok_or_else(|| Error::UnsupportedPlatform {
                    os: format!("{:?}", ctx.platform.os),
                    arch: format!("{:?}", ctx.platform.arch),
                })?;
            let sources = crate::source::select::ranked_source_list(ctx, self).await?;
            let dist = crate::npm::resolve_dist(ctx, &sources, package, &tv.version).await?;
            InstallPlan {
                tool: self.id().to_string(),
                version: tv.version.clone(),
                urls: dist.urls,
                file_name: format!("deno-{}.tgz", tv.version),
                kind: ArchiveKind::TarGz,
                checksum: dist.checksum,
                strip_root: true,
            }
        };
        let pctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
            offline: ctx.config.settings.offline,
            require_checksums: ctx.config.settings.require_checksums,
        };
        pipeline::run(&plan, &pctx).await?;

        // The extracted `deno` needs the executable bit (zip may not preserve it).
        ensure_executable(
            &ctx.dirs.install_path(self.id(), &tv.version),
            ctx.platform.os,
        );
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        // The binary sits at the install root (zip has no bin/ dir).
        Ok(vec![ctx.dirs.install_path(self.id(), &tv.version)])
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            Ok(vec!["deno".into()])
        } else {
            Ok(discovered)
        }
    }

    fn idiomatic_files(&self) -> &[&str] {
        &[".dvmrc"]
    }
}

/// Give the extracted `deno` binary the executable bit on Unix.
fn ensure_executable(install_dir: &std::path::Path, os: Os) {
    if os == Os::Windows {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = install_dir.join("deno");
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
        assert_eq!(
            DenoBackend::platform_package(&ctx(Platform {
                os: Os::Linux,
                arch: Arch::X64,
                libc: Libc::Glibc,
            })),
            Some("@deno/linux-x64-glibc")
        );
        assert_eq!(
            DenoBackend::platform_package(&ctx(Platform {
                os: Os::Linux,
                arch: Arch::X64,
                libc: Libc::Musl,
            })),
            None
        );
    }

    fn ctx(platform: Platform) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some("/tmp/osdk-deno-test/data".into()),
            "OSDK_CACHE_DIR" => Some("/tmp/osdk-deno-test/cache".into()),
            "OSDK_CONFIG_DIR" => Some("/tmp/osdk-deno-test/config".into()),
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
