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

    fn version_info(version: String) -> VersionInfo {
        VersionInfo {
            stable: semver::Version::parse(&version)
                .map(|version| version.pre.is_empty())
                .unwrap_or(false),
            version,
            lts: None,
        }
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
        Ok(versions.into_iter().map(Self::version_info).collect())
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let plan = if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
            plan
        } else {
            let pkg = Self::platform_package(ctx).ok_or_else(|| Error::UnsupportedPlatform {
                os: format!("{:?}", ctx.platform.os),
                arch: format!("{:?}", ctx.platform.arch),
            })?;
            let sources = crate::source::select::ranked_source_list(ctx, self).await?;
            let dist = crate::npm::resolve_dist(ctx, &sources, pkg, &tv.version).await?;
            InstallPlan {
                tool: self.id().to_string(),
                version: tv.version.clone(),
                urls: dist.urls,
                file_name: format!("pnpm-{}.tgz", tv.version),
                kind: ArchiveKind::TarGz,
                checksum: dist.checksum,
                strip_root: true,
                subdir: None,
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

    fn exec_env(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let mapping = cache_mapping(&tv.version);
        Ok(crate::cache::manager_exec_env(
            &ctx.dirs.cache,
            &[("PNPM_HOME", "pnpm"), mapping],
        ))
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        Ok(exposed_bin_names(crate::backend::bin_names_in_dirs(&paths)))
    }
}

fn exposed_bin_names(discovered: Vec<String>) -> Vec<String> {
    let mut names = discovered
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    names.extend(["pnpm".into(), "pnpx".into()]);
    names.into_iter().collect()
}

fn major_version(version: &str) -> u64 {
    version
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0)
}

fn cache_mapping(version: &str) -> (&'static str, &'static str) {
    if major_version(version) >= 11 {
        ("pnpm_config_store_dir", "pnpm-store")
    } else {
        ("npm_config_store_dir", "pnpm-store")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::{select_version, VersionSpec};

    #[test]
    fn latest_ignores_newer_prerelease_versions() {
        let versions = ["11.22.0", "12.0.0-alpha.21"]
            .into_iter()
            .map(|version| PnpmBackend::version_info(version.into()))
            .collect::<Vec<_>>();

        assert_eq!(
            select_version(&VersionSpec::Latest, &versions)
                .unwrap()
                .version,
            "11.22.0"
        );
    }

    #[test]
    fn selects_version_specific_store_environment_key() {
        assert_eq!(
            cache_mapping("10.15.0"),
            ("npm_config_store_dir", "pnpm-store")
        );
        assert_eq!(
            cache_mapping("11.0.0-rc.1"),
            ("pnpm_config_store_dir", "pnpm-store")
        );
        assert_eq!(
            cache_mapping("v12.1.0"),
            ("pnpm_config_store_dir", "pnpm-store")
        );
    }

    #[test]
    fn exposes_pnpx_as_a_routing_alias() {
        assert_eq!(
            exposed_bin_names(vec!["pnpm".into()]),
            vec!["pnpm".to_string(), "pnpx".to_string()]
        );
    }
}
