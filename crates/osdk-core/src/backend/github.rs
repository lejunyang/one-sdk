//! Generic GitHub-release backend, addressed as `github:owner/repo`.
//!
//! Downloads a release asset matching the host platform and installs it. Two
//! asset shapes are handled: archives (tar.*/zip → extracted) and bare binaries
//! (installed directly into `bin/`). Version specs map to release tags
//! (`latest` → the latest release). Mirrors: a CN GitHub proxy fallback.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Os};
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo, VersionSpec};

/// A github backend bound to a specific `owner/repo`.
pub struct GithubBackend {
    /// The full addressed id, e.g. "github:cli/cli".
    id: String,
    owner: String,
    repo: String,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize, Clone)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

impl GithubBackend {
    /// Parse a `github:owner/repo` id into a backend. Returns None if the id
    /// doesn't carry a valid owner/repo.
    pub fn from_id(id: &str) -> Option<GithubBackend> {
        let rest = id.strip_prefix("github:")?;
        let (owner, repo) = rest.split_once('/')?;
        let owner = owner.trim();
        let repo = repo.trim().trim_end_matches(".git");
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(GithubBackend {
            id: id.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }

    fn releases_api(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/releases?per_page=30",
            self.owner, self.repo
        )
    }

    /// Score how well an asset name matches the host platform. Higher is better;
    /// None means it clearly doesn't match (wrong os/arch).
    fn score_asset(&self, name: &str, ctx: &Ctx) -> Option<i32> {
        let n = name.to_ascii_lowercase();
        // Skip checksums/signatures/source archives.
        if n.ends_with(".sha256")
            || n.ends_with(".asc")
            || n.ends_with(".sig")
            || n.ends_with(".pem")
            || n.contains("sha256sums")
            || n.contains("checksums")
        {
            return None;
        }

        let os_ok = match ctx.platform.os {
            Os::Linux => n.contains("linux"),
            Os::Macos => {
                n.contains("darwin")
                    || n.contains("macos")
                    || n.contains("apple")
                    || n.contains("osx")
            }
            Os::Windows => n.contains("windows") || n.contains("win") || n.ends_with(".exe"),
        };
        // Some assets omit OS (bare binaries); allow but score lower.
        let mut score = 0;
        if os_ok {
            score += 10;
        } else if mentions_other_os(&n, ctx.platform.os) {
            return None; // explicitly a different OS
        }

        let arch_ok = match ctx.platform.arch {
            Arch::X64 => n.contains("x86_64") || n.contains("amd64") || n.contains("x64"),
            Arch::Arm64 => n.contains("aarch64") || n.contains("arm64"),
            Arch::X86 => n.contains("i686") || n.contains("i386") || n.contains("x86"),
            Arch::Arm => n.contains("armv7") || n.contains("armhf") || n.contains("arm"),
        };
        if arch_ok {
            score += 10;
        } else if mentions_other_arch(&n, ctx.platform.arch) {
            return None;
        }

        // Prefer archives we can extract; then musl/gnu preferences on linux.
        if ArchiveKind::from_name(&n).is_ok() {
            score += 3;
        }
        if ctx.platform.os == Os::Linux {
            if n.contains("musl") {
                score += 1; // static, more portable
            }
            if n.contains("gnu") {
                score += 1;
            }
        }
        Some(score)
    }
}

#[async_trait]
impl Backend for GithubBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("github", "https://github.com/").with_index("https://api.github.com/"),
            Source::mirror("ghproxy", "https://gh-proxy.com/", 10)
                .with_index("https://api.github.com/"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, _source: &Source) -> Option<String> {
        // Probing the API is rate-limited; skip (selection falls back to order).
        None
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let releases: Vec<GhRelease> =
            http::get_github_json(&ctx.client, &self.releases_api()).await?;
        let mut out: Vec<VersionInfo> = releases
            .into_iter()
            .filter(|r| !r.draft)
            .map(|r| VersionInfo {
                version: r.tag_name.trim_start_matches('v').to_string(),
                stable: !r.prerelease,
                lts: None,
            })
            .filter(|v| !v.version.is_empty())
            .collect();
        // API returns newest-first; want oldest-first.
        out.reverse();
        Ok(out)
    }

    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        // For github, an exact tag passes through; otherwise resolve against the
        // release list (latest/prefix).
        if let VersionSpec::Exact(v) = &req.spec {
            let mut tv = ToolVersion::new(self.id(), v.clone());
            tv.options = req.options.clone();
            return Ok(tv);
        }
        let versions = self.list_remote_versions(ctx).await?;
        let chosen = crate::version::select_version(&req.spec, &versions).ok_or_else(|| {
            Error::VersionResolve {
                tool: self.id().to_string(),
                spec: req.spec.to_string(),
                hint: Some("no matching release".into()),
            }
        })?;
        let mut tv = ToolVersion::new(self.id(), chosen.version.clone());
        tv.options = req.options.clone();
        Ok(tv)
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        // Find the release for this version (try both `v`-prefixed and bare tag).
        let releases: Vec<GhRelease> =
            http::get_github_json(&ctx.client, &self.releases_api()).await?;
        let want = tv.version.trim_start_matches('v');
        let release = releases
            .into_iter()
            .find(|r| r.tag_name.trim_start_matches('v') == want)
            .ok_or_else(|| Error::VersionResolve {
                tool: self.id().to_string(),
                spec: tv.version.clone(),
                hint: Some("release tag not found".into()),
            })?;

        // Pick the best-scoring asset for this platform.
        let mut best: Option<(i32, GhAsset)> = None;
        for asset in &release.assets {
            if let Some(score) = self.score_asset(&asset.name, ctx) {
                if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                    best = Some((score, asset.clone()));
                }
            }
        }
        let (_, asset) = best.ok_or_else(|| {
            Error::Other(format!(
                "no release asset for {} matches this platform ({})",
                self.id(),
                ctx.platform
            ))
        })?;

        // Candidate URLs with a CN proxy fallback.
        let urls = vec![
            asset.browser_download_url.clone(),
            format!("https://gh-proxy.com/{}", asset.browser_download_url),
        ];

        // Archive vs bare binary.
        match ArchiveKind::from_name(&asset.name) {
            Ok(kind) => {
                let plan = InstallPlan {
                    tool: self.id().to_string(),
                    version: tv.version.clone(),
                    urls,
                    file_name: asset.name.clone(),
                    kind,
                    checksum: None,
                    // Some archives have a top dir, some don't; strip only when a
                    // single root dir is present (extract handles the no-op).
                    strip_root: true,
                };
                let pctx = PipelineCtx {
                    client: &ctx.client,
                    dirs: &ctx.dirs,
                    cas: &ctx.cas,
                    link_mode: ctx.config.settings.link_mode,
                    show_progress: ctx.show_progress,
                };
                pipeline::run(&plan, &pctx).await?;
            }
            Err(_) => {
                // Treat as a bare executable named after the repo.
                let exe_name = self.repo.clone();
                crate::backend::pnpm::install_single_binary(
                    ctx,
                    self.id(),
                    &tv.version,
                    &urls,
                    &exe_name,
                    &asset.name,
                    ctx.platform.os,
                )
                .await?;
            }
        }
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        // Archives may put binaries at root or in bin/; expose both.
        let bin = root.join("bin");
        if bin.exists() {
            Ok(vec![bin, root])
        } else {
            Ok(vec![root])
        }
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            Ok(vec![self.repo.clone()])
        } else {
            Ok(discovered)
        }
    }
}

fn mentions_other_os(name: &str, os: Os) -> bool {
    let others: &[&str] = match os {
        Os::Linux => &["darwin", "apple", "macos", "windows", ".exe"],
        Os::Macos => &["linux", "windows", ".exe"],
        Os::Windows => &["linux", "darwin", "apple", "macos"],
    };
    others.iter().any(|o| name.contains(o))
}

fn mentions_other_arch(name: &str, arch: Arch) -> bool {
    let others: &[&str] = match arch {
        Arch::X64 => &["aarch64", "arm64"],
        Arch::Arm64 => &["x86_64", "amd64"],
        Arch::X86 => &["aarch64", "arm64", "x86_64", "amd64"],
        Arch::Arm => &["aarch64", "x86_64", "amd64"],
    };
    others.iter().any(|o| name.contains(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id() {
        let b = GithubBackend::from_id("github:cli/cli").unwrap();
        assert_eq!(b.owner, "cli");
        assert_eq!(b.repo, "cli");
        assert_eq!(b.id(), "github:cli/cli");
        assert!(GithubBackend::from_id("github:noslash").is_none());
        assert!(GithubBackend::from_id("node").is_none());
    }
}
