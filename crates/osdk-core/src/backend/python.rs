//! Python backend: installs prebuilt CPython from astral-sh/python-build-
//! standalone (the same source uv/mise use). Picks the `install_only` archive
//! for the host target triple. No source builds.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct PythonBackend;

/// A GitHub release with its assets.
#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

impl PythonBackend {
    /// Match an `install_only` asset for a given python version + host triple.
    /// Asset names look like:
    ///   cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only.tar.gz
    fn asset_matches(name: &str, py_version: &str, triple: &str) -> bool {
        name.starts_with(&format!("cpython-{py_version}+"))
            && name.contains(triple)
            && name.contains("install_only")
            && !name.contains("install_only_stripped")
            && (name.ends_with(".tar.gz") || name.ends_with(".tar.zst"))
    }

    /// Extract the python version (e.g. "3.12.7") from an asset name.
    fn version_from_asset(name: &str) -> Option<String> {
        // cpython-<ver>+<date>-...
        let rest = name.strip_prefix("cpython-")?;
        let ver = rest.split('+').next()?;
        if ver.split('.').count() >= 2 {
            Some(ver.to_string())
        } else {
            None
        }
    }
}

#[async_trait]
impl Backend for PythonBackend {
    fn id(&self) -> &str {
        "python"
    }

    fn aliases(&self) -> &[&str] {
        &["py", "cpython"]
    }

    fn default_sources(&self) -> Vec<Source> {
        // The index is the GitHub releases API; downloads come from GitHub
        // release asset URLs (which the asset carries in full). Mirrors here are
        // GitHub proxy prefixes applied to the asset URL at download time.
        vec![
            Source::official("github", "https://github.com/")
                .with_index("https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=10"),
            // A GitHub download proxy: download_url is a prefix prepended to the
            // full asset URL. Kept for CN reachability; disabled paths fail over.
            Source::mirror("ghproxy", "https://gh-proxy.com/", 10)
                .with_index("https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=10"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let releases = self.fetch_releases(ctx).await?;
        let triple = ctx.platform.llvm_triple();
        use std::collections::BTreeSet;
        let mut versions: BTreeSet<String> = BTreeSet::new();
        for rel in &releases {
            for asset in &rel.assets {
                if asset.name.contains(&triple)
                    && asset.name.contains("install_only")
                    && !asset.name.contains("install_only_stripped")
                {
                    if let Some(v) = Self::version_from_asset(&asset.name) {
                        versions.insert(v);
                    }
                }
            }
        }
        // Sort ascending by semver-ish ordering.
        let mut out: Vec<VersionInfo> = versions
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

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let triple = ctx.platform.llvm_triple();
        let releases = self.fetch_releases(ctx).await?;

        // Find the newest release asset matching this exact python version.
        let mut chosen: Option<GhAsset> = None;
        for rel in &releases {
            for asset in &rel.assets {
                if Self::asset_matches(&asset.name, &tv.version, &triple) {
                    chosen = Some(asset.clone());
                    break;
                }
            }
            if chosen.is_some() {
                break;
            }
        }
        let asset = chosen.ok_or_else(|| Error::VersionResolve {
            tool: self.id().to_string(),
            spec: tv.version.clone(),
            hint: Some(format!("no install_only asset for {triple}")),
        })?;

        // Build candidate URLs: the plain asset URL, plus any proxy-prefixed
        // variants from mirror sources.
        let mut urls = vec![asset.browser_download_url.clone()];
        for s in &sources {
            if s.id == "github" || s.download_url == "https://github.com/" {
                continue;
            }
            urls.push(http::join_url(
                s.download_url.trim_end_matches('/'),
                &asset.browser_download_url,
            ));
        }

        let kind = ArchiveKind::from_name(&asset.name)?;
        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: tv.version.clone(),
            urls,
            file_name: asset.name.clone(),
            kind,
            checksum: None,   // PBS publishes sha256 sidecars; add later
            strip_root: true, // archives wrap in a `python/` dir
        };
        let pctx = PipelineCtx {
            client: &ctx.client,
            dirs: &ctx.dirs,
            cas: &ctx.cas,
            link_mode: ctx.config.settings.link_mode,
            show_progress: ctx.show_progress,
        };
        pipeline::run(&plan, &pctx).await?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        // PBS layout after stripping `python/`: bin/ on unix, root on windows.
        let dir = match ctx.platform.os {
            Os::Windows => root,
            _ => root.join("bin"),
        };
        Ok(vec![dir])
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            Ok(vec![
                "python".into(),
                "python3".into(),
                "pip".into(),
                "pip3".into(),
            ])
        } else {
            Ok(discovered)
        }
    }

    fn idiomatic_files(&self) -> &[&str] {
        &[".python-version"]
    }
}

impl PythonBackend {
    async fn fetch_releases(&self, ctx: &Ctx) -> Result<Vec<GhRelease>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut last_err: Option<Error> = None;
        for source in &sources {
            let index_url = match &source.index_url {
                Some(u) => u.clone(),
                None => continue,
            };
            match http::get_json::<Vec<GhRelease>>(&ctx.client, &index_url).await {
                Ok(mut rels) => {
                    // newest-first from GitHub; keep stable releases first but
                    // don't drop prereleases entirely.
                    rels.retain(|r| !r.tag_name.is_empty());
                    let _ = &rels.iter().filter(|r| !r.prerelease).count();
                    return Ok(rels);
                }
                Err(e) => {
                    tracing::warn!(source = %source.id, "pbs releases fetch failed: {e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id().to_string(),
            tried: sources.len(),
        }))
    }
}

/// Compare two dotted numeric versions ascending.
pub fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let pb: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
    pa.cmp(&pb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_matching() {
        let name = "cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only.tar.gz";
        assert!(PythonBackend::asset_matches(
            name,
            "3.12.7",
            "x86_64-unknown-linux-gnu"
        ));
        assert!(!PythonBackend::asset_matches(
            name,
            "3.12.7",
            "aarch64-apple-darwin"
        ));
        // stripped variant must not match
        let stripped =
            "cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz";
        assert!(!PythonBackend::asset_matches(
            stripped,
            "3.12.7",
            "x86_64-unknown-linux-gnu"
        ));
    }

    #[test]
    fn version_from_asset_name() {
        let name = "cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only.tar.gz";
        assert_eq!(
            PythonBackend::version_from_asset(name).as_deref(),
            Some("3.12.7")
        );
    }

    #[test]
    fn version_ordering() {
        assert_eq!(cmp_versions("3.9.1", "3.12.0"), std::cmp::Ordering::Less);
        assert_eq!(cmp_versions("3.12.7", "3.12.7"), std::cmp::Ordering::Equal);
    }
}
