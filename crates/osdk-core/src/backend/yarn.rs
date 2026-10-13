//! yarn backend: installs both yarn lines natively (no corepack), verified by
//! the npm registry's Subresource Integrity (SRI).
//!
//! - classic (1.x): the `yarn` npm package
//! - berry (2+): the `@yarnpkg/cli-dist` npm package (the same packaged bundle
//!   corepack uses)
//!
//! yarn is a JavaScript bundle that runs on Node, so we generate small launchers
//! that run the extracted CLI entry with the active node.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct YarnBackend;

impl YarnBackend {
    /// The npm package a yarn version ships in: classic (1.x) -> `yarn`,
    /// berry (2+) -> `@yarnpkg/cli-dist`.
    fn npm_package(version: &str) -> &'static str {
        let major = version
            .split('.')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1);
        if major >= 2 {
            "@yarnpkg/cli-dist"
        } else {
            "yarn"
        }
    }
}

#[async_trait]
impl Backend for YarnBackend {
    fn id(&self) -> &str {
        "yarn"
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::mirror("npmmirror", "https://registry.npmmirror.com/", 5)
                .with_index("https://registry.npmmirror.com/yarn"),
            Source::official("npm", "https://registry.npmjs.org/")
                .with_index("https://registry.npmjs.org/yarn"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    #[cfg(feature = "install")]
    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        // Merge classic (`yarn`) and berry (`@yarnpkg/cli-dist`) version lines.
        use std::collections::BTreeSet;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut set: BTreeSet<String> = BTreeSet::new();
        if let Ok(classic) = crate::npm::list_versions(ctx, &sources, "yarn").await {
            for v in classic {
                if v.starts_with('1') || v.starts_with('0') {
                    set.insert(v);
                }
            }
        }
        if let Ok(berry) = crate::npm::list_versions(ctx, &sources, "@yarnpkg/cli-dist").await {
            for v in berry {
                // stable berry tags only (skip git snapshots like 4.9.1-git.*)
                if !v.contains('-') {
                    set.insert(v);
                }
            }
        }
        let mut out: Vec<VersionInfo> = set
            .into_iter()
            .map(|v| VersionInfo {
                version: v,
                stable: true,
                lts: None,
            })
            .collect();
        out.sort_by(|a, b| crate::backend::python::cmp_versions(&a.version, &b.version));
        Ok(out)
    }

    #[cfg(feature = "install")]
    async fn install_inner(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let plan = if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
            plan
        } else {
            let package = Self::npm_package(&tv.version);
            let sources = crate::source::select::ranked_source_list(ctx, self).await?;
            let dist = crate::npm::resolve_dist(ctx, &sources, package, &tv.version).await?;
            InstallPlan {
                tool: self.id().to_string(),
                version: tv.version.clone(),
                urls: dist.urls,
                file_name: format!("yarn-{}.tgz", tv.version),
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

        // Generate node launchers pointing at the extracted CLI entry. Both
        // `yarn` (classic) and `@yarnpkg/cli-dist` (berry) ship `bin/yarn.js`.
        let entry = if install_dir.join("bin/yarn.js").is_file() {
            install_dir.join("bin/yarn.js")
        } else {
            install_dir.join("lib/cli.js")
        };
        let bin_dir = install_dir.join("bin");
        crate::dirs::create_dir_all(&bin_dir)?;
        write_launcher(&bin_dir, &entry, ctx.platform.os)?;
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
        let mut env = std::collections::BTreeMap::new();
        // Redirect the global folder for both classic (`yarn global add`) and
        // berry so global bins stay under osdk control at
        // <folder>/node_modules/.bin, instead of escaping into yarn's own dir.
        env.insert(
            "YARN_GLOBAL_FOLDER".to_string(),
            ctx.dirs.yarn_global_folder().display().to_string(),
        );
        // Classic honors a disposable YARN_CACHE_FOLDER; berry drives its cache
        // through project/.yarn configuration, not this variable.
        if yarn_major(&tv.version) < 2 {
            let force = ctx.config.settings.effective_override_env(self.id());
            env.extend(crate::cache::manager_exec_env(
                &ctx.dirs.cache,
                &[("YARN_CACHE_FOLDER", "yarn-classic")],
                force,
            ));
        }
        Ok(env)
    }

    fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(vec!["yarn".into(), "yarnpkg".into()])
    }
}

fn yarn_major(version: &str) -> u64 {
    version
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(1)
}

#[cfg(unix)]
fn write_launcher(bin_dir: &std::path::Path, js: &std::path::Path, _os: Os) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for name in ["yarn", "yarnpkg"] {
        let p = bin_dir.join(name);
        let script = format!("#!/bin/sh\nexec node \"{}\" \"$@\"\n", js.display());
        std::fs::write(&p, script).map_err(|e| Error::io(&p, e))?;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

#[cfg(windows)]
fn write_launcher(bin_dir: &std::path::Path, js: &std::path::Path, _os: Os) -> Result<()> {
    for name in ["yarn", "yarnpkg"] {
        let p = bin_dir.join(format!("{name}.cmd"));
        let script = format!("@echo off\r\nnode \"{}\" %*\r\n", js.display());
        std::fs::write(&p, script).map_err(|e| Error::io(&p, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_classic_vs_berry_to_npm_package() {
        assert_eq!(YarnBackend::npm_package("1.22.22"), "yarn");
        assert_eq!(YarnBackend::npm_package("2.4.3"), "@yarnpkg/cli-dist");
        assert_eq!(YarnBackend::npm_package("4.10.3"), "@yarnpkg/cli-dist");
        // malformed -> classic default
        assert_eq!(YarnBackend::npm_package("weird"), "yarn");
    }

    #[test]
    fn redirects_global_folder_for_classic_and_berry() {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some("/tmp/osdk-yarn-test/data".into()),
            "OSDK_CACHE_DIR" => Some("/tmp/osdk-yarn-test/cache".into()),
            "OSDK_CONFIG_DIR" => Some("/tmp/osdk-yarn-test/config".into()),
            _ => None,
        })
        .unwrap();
        let ctx = Ctx {
            dirs: dirs.clone(),
            platform: crate::platform::Platform {
                os: crate::platform::Os::Linux,
                arch: crate::platform::Arch::X64,
                libc: crate::platform::Libc::Glibc,
            },
            config: crate::config::Config::default(),
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store)),
            show_progress: false,
        };

        // Classic (1.x): global folder redirected and a disposable cache set.
        let classic = ToolVersion::new(YarnBackend.id(), "1.22.22");
        let env = YarnBackend.exec_env(&ctx, &classic).unwrap();
        assert_eq!(
            std::path::PathBuf::from(env.get("YARN_GLOBAL_FOLDER").unwrap().clone()),
            ctx.dirs.yarn_global_folder()
        );
        assert!(env.contains_key("YARN_CACHE_FOLDER"));

        // Berry (4.x): global folder redirected, no YARN_CACHE_FOLDER.
        let berry = ToolVersion::new(YarnBackend.id(), "4.10.3");
        let envb = YarnBackend.exec_env(&ctx, &berry).unwrap();
        assert_eq!(
            std::path::PathBuf::from(envb.get("YARN_GLOBAL_FOLDER").unwrap().clone()),
            ctx.dirs.yarn_global_folder()
        );
        assert!(!envb.contains_key("YARN_CACHE_FOLDER"));
    }
}
