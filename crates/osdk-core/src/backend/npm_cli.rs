use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct NpmBackend;

/// Persist the managed default in npm's own configuration, so invoking the
/// original npm without a shell hook has the same global install location.
/// User config, environment and CLI options retain npm's native precedence.
pub(crate) fn configure_global_prefix(
    ctx: &Ctx,
    install_root: &std::path::Path,
    npm_root: &std::path::Path,
) -> Result<()> {
    use crate::store::manifest::{FileEntry, Manifest};
    if !npm_root.is_dir() {
        return Ok(());
    }
    let _lock = crate::lock::FileLock::acquire(ctx.dirs.data.join("locks/npm-defaults.lock"))?;
    let path = npm_root.join("npmrc");
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(Error::io(&path, error)),
    };
    const COMMENT: &str = "# osdk: default global prefix (user config and --prefix override this)";
    let mut text = String::new();
    for line in original.lines() {
        if line == COMMENT
            || line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "prefix")
        {
            continue;
        }
        text.push_str(line);
        text.push('\n');
    }
    text.push_str(COMMENT);
    text.push_str("\nprefix=");
    text.push_str(&serde_json::to_string(
        &ctx.dirs.npm_global_prefix().to_string_lossy(),
    )?);
    text.push('\n');
    if text != original {
        // Update only a clean, previously recorded entry. Re-recording the
        // whole tree here would silently bless pre-existing binary or npmrc
        // modifications. Missing/corrupt old manifests remain unverifiable.
        let relative = path
            .strip_prefix(install_root)
            .map_err(|error| Error::other(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let baseline = Manifest::load(install_root)
            .ok()
            .filter(|manifest| matches!(manifest.tool.as_str(), "node" | "npm"));
        let mut updated = None;
        if let Some(mut manifest) = baseline {
            let expected = blake3::hash(original.as_bytes()).to_hex().to_string();
            let new_hash = blake3::hash(text.as_bytes()).to_hex().to_string();
            if let Some(entry) = manifest
                .files
                .iter_mut()
                .find(|entry| entry.path == relative)
            {
                if entry.symlink.is_none()
                    && entry.hash.as_deref() == Some(expected.as_str())
                    && std::fs::symlink_metadata(&path)
                        .is_ok_and(|metadata| metadata.file_type().is_file())
                {
                    entry.hash = Some(new_hash);
                    updated = Some(manifest);
                }
            } else if std::fs::symlink_metadata(&path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                manifest.files.push(FileEntry {
                    path: relative,
                    hash: Some(new_hash),
                    mode: 0,
                    symlink: None,
                });
                updated = Some(manifest);
            }
        }
        // Atomic replacement breaks CAS hard links instead of modifying a
        // shared blob in place. Do not rewrite unchanged files on every run.
        crate::fs::write_atomic(&path, text.as_bytes()).map_err(|error| Error::io(&path, error))?;
        if let Some(manifest) = updated {
            let manifest_path = Manifest::manifest_path(install_root);
            crate::fs::write_atomic(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)
                .map_err(|error| Error::io(&manifest_path, error))?;
        }
    }
    Ok(())
}

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

    #[cfg(feature = "install")]
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

    #[cfg(feature = "install")]
    async fn install_inner(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
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
        configure_global_prefix(ctx, &install, &install)?;
        Ok(())
    }

    fn ensure_post_install(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        configure_global_prefix(ctx, &root, &root)
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
        _tv: &ToolVersion,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let force = ctx.config.settings.effective_override_env(self.id());
        let mut env =
            crate::cache::manager_exec_env(&ctx.dirs.cache, &[("npm_config_cache", "npm")], force);
        // Redirect native global installs to the controlled prefix regardless
        // of an ambient npm_config_prefix: this is a location, not a cache, and
        // it is what makes a direct `npm i -g` land under osdk.
        env.insert(
            "npm_config_prefix".into(),
            ctx.dirs.npm_global_prefix().display().to_string(),
        );
        Ok(env)
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
