use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct NpmBackend;

#[async_trait]
impl Backend for NpmBackend {
    fn id(&self) -> &str {
        "npm"
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::mirror("npmmirror", "https://registry.npmmirror.com/", 5)
                .with_index("https://registry.npmmirror.com/npm"),
            Source::official("npm", "https://registry.npmjs.org/")
                .with_index("https://registry.npmjs.org/npm"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let versions = crate::npm::list_versions(ctx, &sources, "npm").await?;
        Ok(versions
            .into_iter()
            .map(|version| VersionInfo {
                stable: !version.contains('-'),
                version,
                lts: None,
            })
            .collect())
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let plan = if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
            plan
        } else {
            let sources = crate::source::select::ranked_source_list(ctx, self).await?;
            let dist = crate::npm::resolve_dist(ctx, &sources, "npm", &tv.version).await?;
            InstallPlan {
                tool: self.id().into(),
                version: tv.version.clone(),
                urls: dist.urls,
                file_name: format!("npm-{}.tgz", tv.version),
                kind: ArchiveKind::TarGz,
                checksum: dist.checksum,
                strip_root: true,
                subdir: None,
            }
        };
        let pipeline_ctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
            offline: ctx.config.settings.offline,
            require_checksums: true,
        };
        let install = pipeline::run(&plan, &pipeline_ctx).await?;
        let entry = install.join("bin/npm-cli.js");
        let npx_entry = install.join("bin/npx-cli.js");
        if !entry.is_file() || !npx_entry.is_file() {
            let _ = std::fs::remove_dir_all(&install);
            return Err(Error::other(format!(
                "npm {} archive is missing bin/npm-cli.js or bin/npx-cli.js",
                tv.version
            )));
        }
        write_launchers(&install.join("bin"), &entry, &npx_entry, ctx.platform.os)?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![ctx
            .dirs
            .install_path(self.id(), &tv.version)
            .join("bin")])
    }

    fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(vec!["npm".into(), "npx".into()])
    }
}

#[cfg(unix)]
fn write_launchers(
    bin_dir: &std::path::Path,
    npm: &std::path::Path,
    npx: &std::path::Path,
    _os: Os,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for (name, script) in [("npm", npm), ("npx", npx)] {
        let path = bin_dir.join(name);
        let contents = format!("#!/bin/sh\nexec node \"{}\" \"$@\"\n", script.display());
        std::fs::write(&path, contents).map_err(|error| Error::io(&path, error))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| Error::io(&path, error))?;
    }
    Ok(())
}

#[cfg(windows)]
fn write_launchers(
    bin_dir: &std::path::Path,
    npm: &std::path::Path,
    npx: &std::path::Path,
    _os: Os,
) -> Result<()> {
    for (name, script) in [("npm", npm), ("npx", npx)] {
        let path = bin_dir.join(format!("{name}.cmd"));
        let contents = format!("@echo off\r\nnode \"{}\" %*\r\n", script.display());
        std::fs::write(&path, contents).map_err(|error| Error::io(&path, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn launchers_call_node_from_path() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let npm = bin.join("npm-cli.js");
        let npx = bin.join("npx-cli.js");
        std::fs::write(&npm, "").unwrap();
        std::fs::write(&npx, "").unwrap();
        write_launchers(&bin, &npm, &npx, Os::Linux).unwrap();
        let launcher = std::fs::read_to_string(bin.join("npm")).unwrap();
        assert!(launcher.contains("exec node"));
        assert!(launcher.contains("npm-cli.js"));
    }
}
