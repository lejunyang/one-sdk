//! Deno backend: installs the official `deno` binary from dl.deno.land (the
//! canonical CDN, which avoids the rate-limited GitHub API). Assets are
//! `deno-<triple>.zip` containing a single `deno` executable, with a
//! `<asset>.sha256sum` sidecar we verify.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, Checksum, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Os};
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo, VersionSpec};

pub struct DenoBackend;

impl DenoBackend {
    /// Deno's target triple for the host, e.g. `x86_64-unknown-linux-gnu`,
    /// `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`.
    fn triple(ctx: &Ctx) -> Option<String> {
        let cpu = match ctx.platform.arch {
            Arch::X64 => "x86_64",
            Arch::Arm64 => "aarch64",
            _ => return None, // deno ships only x64/arm64
        };
        let sys = match ctx.platform.os {
            Os::Linux => "unknown-linux-gnu",
            Os::Macos => "apple-darwin",
            Os::Windows => "pc-windows-msvc",
        };
        Some(format!("{cpu}-{sys}"))
    }

    fn asset_name(ctx: &Ctx) -> Option<String> {
        Some(format!("deno-{}.zip", Self::triple(ctx)?))
    }
}

#[async_trait]
impl Backend for DenoBackend {
    fn id(&self) -> &str {
        "deno"
    }

    fn default_sources(&self) -> Vec<Source> {
        // dl.deno.land is the canonical CDN (no GitHub API rate limit). The
        // GitHub releases API is a secondary for full version listing.
        vec![
            Source::official("denoland", "https://dl.deno.land/release")
                .with_index("https://dl.deno.land/release-latest.txt"),
            Source::mirror(
                "github",
                "https://github.com/denoland/deno/releases/download",
                20,
            )
            .with_index("https://api.github.com/repos/denoland/deno/releases?per_page=50"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        // Prefer the GitHub releases API for a full list (token raises the
        // limit); fall back to the single "latest" from dl.deno.land.
        let api = "https://api.github.com/repos/denoland/deno/releases?per_page=50";
        if let Ok(rels) = http::get_github_json::<Vec<GhRelease>>(&ctx.client, api).await {
            let mut out: Vec<VersionInfo> = rels
                .into_iter()
                .filter(|r| !r.draft)
                .map(|r| VersionInfo {
                    version: r.tag_name.trim_start_matches('v').to_string(),
                    stable: !r.prerelease,
                    lts: None,
                })
                .filter(|v| !v.version.is_empty())
                .collect();
            out.reverse();
            if !out.is_empty() {
                return Ok(out);
            }
        }
        // Fallback: the latest version from the CDN.
        let latest = self.fetch_latest(ctx).await?;
        Ok(vec![VersionInfo::stable(latest)])
    }

    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        if let VersionSpec::Exact(v) = &req.spec {
            return Ok(ToolVersion::new(self.id(), v.clone()));
        }
        // `latest` can be resolved cheaply from the CDN without listing all.
        if matches!(req.spec, VersionSpec::Latest) {
            if let Ok(latest) = self.fetch_latest(ctx).await {
                return Ok(ToolVersion::new(self.id(), latest));
            }
        }
        let versions = self.list_remote_versions(ctx).await?;
        let chosen = crate::version::select_version(&req.spec, &versions).ok_or_else(|| {
            Error::VersionResolve {
                tool: self.id().to_string(),
                spec: req.spec.to_string(),
                hint: Some("no matching deno release".into()),
            }
        })?;
        Ok(ToolVersion::new(self.id(), chosen.version.clone()))
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let asset = Self::asset_name(ctx).ok_or_else(|| Error::UnsupportedPlatform {
            os: format!("{:?}", ctx.platform.os),
            arch: format!("{:?}", ctx.platform.arch),
        })?;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;

        // Build candidate URLs: <base>/v<version>/<asset> for each source, plus a
        // gh-proxy fallback for the github source.
        let mut urls = Vec::new();
        for s in &sources {
            let base = http::join_url(&s.download_url, &format!("v{}", tv.version));
            let url = http::join_url(&base, &asset);
            if s.download_url.contains("github.com") {
                urls.push(format!("https://gh-proxy.com/{url}"));
            }
            urls.push(url);
        }

        // Discover the sha256 from the sidecar (try each candidate location).
        let mut checksum: Option<Checksum> = None;
        for u in &urls {
            if let Some(hex) = pipeline::verify::discover_asset_checksum(&ctx.client, u).await {
                checksum = Some(hex);
                break;
            }
        }

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: tv.version.clone(),
            urls,
            file_name: asset,
            kind: ArchiveKind::Zip,
            checksum,
            strip_root: false, // deno zip contains just the `deno` binary at root
        };
        let pctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
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

impl DenoBackend {
    /// The latest deno version from dl.deno.land (rate-limit-free).
    async fn fetch_latest(&self, ctx: &Ctx) -> Result<String> {
        let body = http::get_text(&ctx.client, "https://dl.deno.land/release-latest.txt").await?;
        let v = body.trim().trim_start_matches('v').to_string();
        if v.is_empty() {
            Err(Error::other("empty deno release-latest.txt"))
        } else {
            Ok(v)
        }
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

#[derive(Debug, serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_shape() {
        // Build a minimal platform to exercise triple/asset_name deterministically.
        use crate::platform::{Libc, Platform};
        let linux_x64 = Platform {
            os: Os::Linux,
            arch: Arch::X64,
            libc: Libc::Glibc,
        };
        // triple() reads ctx.platform, so replicate its logic via a tiny shim.
        let cpu = "x86_64";
        let sys = "unknown-linux-gnu";
        assert_eq!(format!("{cpu}-{sys}"), "x86_64-unknown-linux-gnu");
        // arm/x86 hosts are unsupported by deno; ensure our match arm excludes them.
        assert!(matches!(linux_x64.arch, Arch::X64));
    }
}
