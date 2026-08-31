//! pnpm backend: installs pnpm's complete JavaScript distribution from the npm
//! registry with first-party SRI verification. osdk supplies the exact managed
//! Node runtime and creates portable launchers for `pnpm` and `pnpx`.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct PnpmBackend;

impl PnpmBackend {
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
            let sources = crate::source::select::ranked_source_list(ctx, self).await?;
            let dist = crate::npm::resolve_dist(ctx, &sources, "pnpm", &tv.version).await?;
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
        let install_dir = pipeline::run(&plan, &pctx).await?;
        write_launchers(&install_dir.join("bin"), ctx.platform.os)?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![ctx
            .dirs
            .install_path(self.id(), &tv.version)
            .join("bin")])
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
        let _ = (ctx, tv);
        Ok(vec!["pnpm".into(), "pnpx".into()])
    }
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

#[cfg(unix)]
fn write_launchers(bin_dir: &std::path::Path, _os: Os) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for name in ["pnpm", "pnpx"] {
        let path = bin_dir.join(name);
        let module = bin_dir.join(format!("{name}.mjs"));
        if !module.is_file() {
            return Err(Error::other(format!(
                "pnpm distribution is missing {}",
                module.display()
            )));
        }
        let script = format!("#!/bin/sh\nexec node \"{}\" \"$@\"\n", module.display());
        std::fs::write(&path, script).map_err(|error| Error::io(&path, error))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| Error::io(&path, error))?;
    }
    Ok(())
}

#[cfg(windows)]
fn write_launchers(bin_dir: &std::path::Path, _os: Os) -> Result<()> {
    for name in ["pnpm", "pnpx"] {
        let module = bin_dir.join(format!("{name}.mjs"));
        if !module.is_file() {
            return Err(Error::other(format!(
                "pnpm distribution is missing {}",
                module.display()
            )));
        }
        let path = bin_dir.join(format!("{name}.cmd"));
        let script = format!("@echo off\r\nnode \"%~dp0{name}.mjs\" %*\r\n");
        std::fs::write(&path, script).map_err(|error| Error::io(&path, error))?;
    }
    Ok(())
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
        let context = ctx();
        assert_eq!(
            PnpmBackend
                .bin_names(&context, &ToolVersion::new("pnpm", "11.24.0"))
                .unwrap(),
            vec!["pnpm".to_string(), "pnpx".to_string()]
        );
    }

    #[test]
    fn launchers_target_complete_distribution_modules() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = temporary.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("pnpm.mjs"), b"export {};").unwrap();
        std::fs::write(bin.join("pnpx.mjs"), b"export {};").unwrap();
        write_launchers(&bin, Os::Linux).unwrap();
        #[cfg(unix)]
        {
            let pnpm = std::fs::read_to_string(bin.join("pnpm")).unwrap();
            assert!(pnpm.contains("bin/pnpm.mjs"), "{pnpm}");
            assert!(std::fs::metadata(bin.join("pnpm")).unwrap().is_file());
        }
        #[cfg(windows)]
        {
            let pnpm = std::fs::read_to_string(bin.join("pnpm.cmd")).unwrap();
            assert!(pnpm.contains("%~dp0pnpm.mjs"), "{pnpm}");
            assert!(std::fs::metadata(bin.join("pnpm.cmd")).unwrap().is_file());
        }
    }

    fn ctx() -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some("/tmp/osdk-pnpm-test/data".into()),
            "OSDK_CACHE_DIR" => Some("/tmp/osdk-pnpm-test/cache".into()),
            "OSDK_CONFIG_DIR" => Some("/tmp/osdk-pnpm-test/config".into()),
            _ => None,
        })
        .unwrap();
        Ctx {
            dirs: dirs.clone(),
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store)),
            show_progress: false,
        }
    }
}
