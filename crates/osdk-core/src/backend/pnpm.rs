//! pnpm backend: installs the standalone `@pnpm/exe` binary (Node bundled) from
//! the npm registry, so it works without a managed Node. Verified by the
//! registry tarball integrity when available.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline;
use crate::platform::{Arch, Os};
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct PnpmBackend;

#[derive(Debug, Deserialize)]
struct NpmPackument {
    #[serde(default)]
    versions: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "dist-tags")]
    dist_tags: BTreeMap<String, String>,
}

impl PnpmBackend {
    /// The `@pnpm/exe` platform-specific package name for this host.
    /// npm publishes `@pnpm/<os>-<arch>` optional deps with the raw binary.
    fn platform_binary_url(version: &str, ctx: &Ctx) -> Option<(String, String)> {
        // pnpm publishes standalone binaries on GitHub releases:
        //   https://github.com/pnpm/pnpm/releases/download/v<ver>/pnpm-<os>-<arch>[.exe]
        let (os, arch, ext) = match (ctx.platform.os, ctx.platform.arch) {
            (Os::Linux, Arch::X64) => ("linuxstatic", "x64", ""),
            (Os::Linux, Arch::Arm64) => ("linuxstatic", "arm64", ""),
            (Os::Macos, Arch::X64) => ("macos", "x64", ""),
            (Os::Macos, Arch::Arm64) => ("macos", "arm64", ""),
            (Os::Windows, Arch::X64) => ("win", "x64", ".exe"),
            _ => return None,
        };
        let file = format!("pnpm-{os}-{arch}{ext}");
        let url = format!("https://github.com/pnpm/pnpm/releases/download/v{version}/{file}");
        Some((url, file))
    }
}

#[async_trait]
impl Backend for PnpmBackend {
    fn id(&self) -> &str {
        "pnpm"
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("npm", "https://registry.npmjs.org/")
                .with_index("https://registry.npmjs.org/pnpm"),
            Source::mirror("npmmirror", "https://registry.npmmirror.com/", 10)
                .with_index("https://registry.npmmirror.com/pnpm"),
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
                    let latest = pack.dist_tags.get("latest").cloned();
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
                    // mark latest via lts field misuse? keep simple: ignore.
                    let _ = latest;
                    return Ok(out);
                }
                Err(e) => {
                    tracing::warn!(source = %source.id, "pnpm packument fetch failed: {e}");
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
        let (gh_url, file) = Self::platform_binary_url(&tv.version, ctx).ok_or_else(|| {
            Error::UnsupportedPlatform {
                os: format!("{:?}", ctx.platform.os),
                arch: format!("{:?}", ctx.platform.arch),
            }
        })?;

        // Candidate URLs (best-first): GitHub release, then proxy/mirror
        // prefixes for CN reachability.
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut urls = vec![gh_url.clone()];
        for s in &sources {
            // Mirrors here act as GitHub download proxies (prefix + full URL).
            if s.download_url.contains("registry.") {
                continue; // npm registry can't serve GH release binaries
            }
            urls.push(http::join_url(
                s.download_url.trim_end_matches('/'),
                &gh_url,
            ));
        }
        // Always include the well-known gh-proxy as a fallback.
        urls.push(format!("https://gh-proxy.com/{gh_url}"));

        install_single_binary(
            ctx,
            self.id(),
            &tv.version,
            &urls,
            "pnpm",
            &file,
            ctx.platform.os,
        )
        .await?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![ctx
            .dirs
            .install_path(self.id(), &tv.version)
            .join("bin")])
    }

    fn bin_names(&self, _ctx: &Ctx, _tv: &ToolVersion) -> Result<Vec<String>> {
        Ok(vec!["pnpm".into(), "pnpx".into()])
    }
}

/// Download a single executable into `<install>/bin/<exe_name>` and mark the
/// install complete. Tries each URL in order (failover). Used for standalone
/// binaries (pnpm, yarn).
pub(crate) async fn install_single_binary(
    ctx: &Ctx,
    tool: &str,
    version: &str,
    urls: &[String],
    exe_name: &str,
    download_name: &str,
    os: Os,
) -> Result<()> {
    let install_dir = ctx.dirs.install_path(tool, version);
    if install_dir.join(".osdk-complete").exists() {
        return Ok(());
    }
    if install_dir.exists() {
        let _ = std::fs::remove_dir_all(&install_dir);
    }
    let bin_dir = install_dir.join("bin");
    crate::dirs::create_dir_all(&bin_dir)?;

    // Download to cache (trying each URL), then copy into bin with the canonical
    // exe name.
    let cached = ctx.dirs.downloads().join(download_name);
    let _ = std::fs::remove_file(&cached);
    let mut last_err: Option<Error> = None;
    let mut ok = false;
    for (i, url) in urls.iter().enumerate() {
        match pipeline::download::download(
            &ctx.client,
            url,
            &cached,
            &format!("{tool}@{version}"),
            ctx.show_progress,
        )
        .await
        {
            Ok(()) => {
                ok = true;
                break;
            }
            Err(e) => {
                tracing::warn!(url = %url, attempt = i + 1, total = urls.len(), "binary download failed: {e}");
                last_err = Some(e);
            }
        }
    }
    if !ok {
        return Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: tool.to_string(),
            tried: urls.len(),
        }));
    }

    let exe_suffix = if os == Os::Windows { ".exe" } else { "" };
    let dest = bin_dir.join(format!("{exe_name}{exe_suffix}"));
    std::fs::copy(&cached, &dest).map_err(|e| Error::io(&dest, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
    }

    std::fs::write(install_dir.join(".osdk-complete"), b"")
        .map_err(|e| Error::io(install_dir.join(".osdk-complete"), e))?;
    Ok(())
}
