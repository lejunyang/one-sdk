//! yarn backend: installs the standalone yarn CLI bundle. Yarn (classic and
//! berry) is distributed as a JavaScript bundle that runs on Node, so this
//! backend fetches the bundle and generates a small launcher that runs it with
//! the active node. Requires a node managed by osdk (or on PATH).

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline;
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct YarnBackend;

#[derive(Debug, Deserialize)]
struct NpmPackument {
    #[serde(default)]
    versions: BTreeMap<String, serde_json::Value>,
}

#[async_trait]
impl Backend for YarnBackend {
    fn id(&self) -> &str {
        "yarn"
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("npm", "https://registry.npmjs.org/")
                .with_index("https://registry.npmjs.org/yarn"),
            Source::mirror("npmmirror", "https://registry.npmmirror.com/", 10)
                .with_index("https://registry.npmmirror.com/yarn"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut last_err: Option<Error> = None;
        for source in &sources {
            let index_url = match &source.index_url {
                Some(u) => u.clone(),
                None => continue,
            };
            match http::get_json::<NpmPackument>(&ctx.client, &index_url).await {
                Ok(pack) => {
                    let mut out: Vec<VersionInfo> = pack
                        .versions
                        .into_keys()
                        .map(|v| VersionInfo {
                            version: v,
                            stable: true,
                            lts: None,
                        })
                        .collect();
                    out.sort_by(|a, b| {
                        crate::backend::python::cmp_versions(&a.version, &b.version)
                    });
                    return Ok(out);
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id().to_string(),
            tried: sources.len(),
        }))
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        // Yarn classic ships a self-contained CLI at
        //   https://github.com/yarnpkg/yarn/releases/download/v<ver>/yarn-<ver>.js
        // Berry (>=2) is distributed differently (per-repo .cjs); for global
        // management we support classic here and recommend corepack for berry.
        let major = tv
            .version
            .split('.')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1);
        if major >= 2 {
            return Err(Error::other(format!(
                "yarn {} (berry) is best managed per-project via corepack; \
                 osdk manages yarn classic (1.x) standalone",
                tv.version
            )));
        }
        let gh_url = format!(
            "https://github.com/yarnpkg/yarn/releases/download/v{v}/yarn-{v}.js",
            v = tv.version
        );
        // Candidate URLs (best-first) with a CN GitHub-proxy fallback, mirroring
        // the pnpm/python/java download-failover pattern.
        let urls = [gh_url.clone(), format!("https://gh-proxy.com/{gh_url}")];

        let install_dir = ctx.dirs.install_path(self.id(), &tv.version);
        if install_dir.join(".osdk-complete").exists() {
            return Ok(());
        }
        if install_dir.exists() {
            let _ = std::fs::remove_dir_all(&install_dir);
        }
        let libexec = install_dir.join("libexec");
        crate::dirs::create_dir_all(&libexec)?;
        let js = libexec.join("yarn.js");
        let mut last_err: Option<Error> = None;
        let mut ok = false;
        for (i, url) in urls.iter().enumerate() {
            match pipeline::download::download(
                &ctx.client,
                url,
                &js,
                &format!("yarn@{}", tv.version),
                ctx.show_progress,
            )
            .await
            {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(url = %url, attempt = i + 1, total = urls.len(), "{}", crate::i18n::trf("log.yarn_download_failed", &[("err", &e.to_string())]));
                    last_err = Some(e);
                }
            }
        }
        if !ok {
            return Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
                tool: self.id().to_string(),
                tried: urls.len(),
            }));
        }

        // Generate a launcher in bin/ that runs the bundle with node.
        let bin_dir = install_dir.join("bin");
        crate::dirs::create_dir_all(&bin_dir)?;
        write_launcher(&bin_dir, &js, ctx.platform.os)?;

        std::fs::write(install_dir.join(".osdk-complete"), b"")
            .map_err(|e| Error::io(install_dir.join(".osdk-complete"), e))?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![ctx
            .dirs
            .install_path(self.id(), &tv.version)
            .join("bin")])
    }

    fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(vec!["yarn".into(), "yarnpkg".into()])
    }
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
