//! Bun backend: installs the official `bun` binary from GitHub releases. Assets
//! are `bun-<os>-<arch>[-musl][-baseline].zip` containing `bun-<os>-<arch>/bun`,
//! verified against the release's shared `SHASUMS256.txt`.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, Checksum, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Libc, Os};
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo, VersionSpec};

pub struct BunBackend;

impl BunBackend {
    /// The bun asset stem for the host, e.g. `bun-linux-x64`, `bun-darwin-aarch64`,
    /// `bun-windows-x64`, plus `-musl` on musl Linux.
    fn asset_stem(ctx: &Ctx) -> Option<String> {
        let os = match ctx.platform.os {
            Os::Linux => "linux",
            Os::Macos => "darwin",
            Os::Windows => "windows",
        };
        let arch = match ctx.platform.arch {
            Arch::X64 => "x64",
            Arch::Arm64 => "aarch64",
            _ => return None, // bun ships only x64/arm64
        };
        let mut stem = format!("bun-{os}-{arch}");
        if ctx.platform.os == Os::Linux && ctx.platform.libc == Libc::Musl {
            stem.push_str("-musl");
        }
        Some(stem)
    }

    fn asset_name(ctx: &Ctx) -> Option<String> {
        Some(format!("{}.zip", Self::asset_stem(ctx)?))
    }

    /// The bun tag for a version, e.g. "1.3.14" -> "bun-v1.3.14".
    fn tag_for(version: &str) -> String {
        format!("bun-v{version}")
    }
}

#[async_trait]
impl Backend for BunBackend {
    fn id(&self) -> &str {
        "bun"
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("github", "https://github.com/oven-sh/bun/releases/download")
                .with_index("https://api.github.com/repos/oven-sh/bun/releases?per_page=50"),
            Source::mirror(
                "ghproxy",
                "https://gh-proxy.com/https://github.com/oven-sh/bun/releases/download",
                10,
            )
            .with_index("https://api.github.com/repos/oven-sh/bun/releases?per_page=50"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let api = "https://api.github.com/repos/oven-sh/bun/releases?per_page=50";
        let rels: Vec<GhRelease> = http::get_github_json(&ctx.client, api).await?;
        let mut out: Vec<VersionInfo> = rels
            .into_iter()
            .filter(|r| !r.draft)
            // bun tags look like `bun-v1.3.14`; canary/nightly excluded here
            .filter_map(|r| {
                let v = r
                    .tag_name
                    .strip_prefix("bun-v")
                    .or_else(|| r.tag_name.strip_prefix('v'))?;
                Some(VersionInfo {
                    version: v.to_string(),
                    stable: !r.prerelease,
                    lts: None,
                })
            })
            .filter(|v| !v.version.is_empty() && v.version.chars().next().unwrap().is_ascii_digit())
            .collect();
        out.reverse();
        Ok(out)
    }

    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        if let VersionSpec::Exact(v) = &req.spec {
            return Ok(ToolVersion::new(self.id(), v.clone()));
        }
        let versions = self.list_remote_versions(ctx).await?;
        let chosen = crate::version::select_version(&req.spec, &versions).ok_or_else(|| {
            Error::VersionResolve {
                tool: self.id().to_string(),
                spec: req.spec.to_string(),
                hint: Some("no matching bun release".into()),
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
        let tag = Self::tag_for(&tv.version);
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;

        // <base>/<tag>/<asset> for each source.
        let urls: Vec<String> = sources
            .iter()
            .map(|s| {
                let base = http::join_url(&s.download_url, &tag);
                http::join_url(&base, &asset)
            })
            .collect();

        // Verify via the release's shared SHASUMS256.txt (matched by filename).
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
            strip_root: true, // zip wraps `bun` in a bun-<os>-<arch>/ dir
        };
        let pctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
        };
        pipeline::run(&plan, &pctx).await?;
        ensure_executable(
            &ctx.dirs.install_path(self.id(), &tv.version),
            ctx.platform.os,
        );
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        Ok(vec![ctx.dirs.install_path(self.id(), &tv.version)])
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            // bun's archive ships a single `bun` binary (bunx is `bun x`).
            Ok(vec!["bun".into()])
        } else {
            Ok(discovered)
        }
    }

    fn idiomatic_files(&self) -> &[&str] {
        &[".bun-version"]
    }
}

fn ensure_executable(install_dir: &std::path::Path, os: Os) {
    if os == Os::Windows {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = install_dir.join("bun");
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
    fn tag_format() {
        assert_eq!(BunBackend::tag_for("1.3.14"), "bun-v1.3.14");
    }
}
