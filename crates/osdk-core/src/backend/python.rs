//! Python backend: installs prebuilt CPython from astral-sh/python-build-
//! standalone (the same source uv/mise use). Discovery uses the project's
//! `latest-release.json` (tag + `asset_url_prefix`) plus the companion
//! `SHA256SUMS` at that prefix — this lists every asset filename with its
//! sha256, so we get both version discovery AND checksums without ever touching
//! the rate-limited GitHub releases API. The catalog is cached (24h TTL) with a
//! stale-cache fallback for offline/flaky networks.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, Checksum, HashAlgo, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolVersion, VersionInfo};

pub struct PythonBackend;

/// The `latest-release.json` metadata document.
#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag: String,
    asset_url_prefix: String,
}

/// A single asset parsed from `SHA256SUMS`: filename + sha256.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Asset {
    name: String,
    sha256: String,
}

/// The cached catalog for one release tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Catalog {
    tag: String,
    asset_url_prefix: String,
    assets: Vec<Asset>,
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
            // exclude the free-threaded ("freethreaded") variants by default
            && !name.contains("freethreaded")
            && (name.ends_with(".tar.gz") || name.ends_with(".tar.zst"))
    }

    /// Whether an asset is an `install_only` build for the host triple (any
    /// version) — used to enumerate installable versions.
    fn asset_is_install_only(name: &str, triple: &str) -> bool {
        name.starts_with("cpython-")
            && name.contains(triple)
            && name.contains("install_only")
            && !name.contains("install_only_stripped")
            && !name.contains("freethreaded")
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
        // `index_url` points at the metadata JSON on raw.githubusercontent (no
        // API rate limit). `download_url` is a prefix prepended to full GitHub
        // asset URLs at download time (proxy for CN reachability).
        vec![
            Source::official("github", "https://github.com/").with_index(
                "https://raw.githubusercontent.com/astral-sh/python-build-standalone/latest-release/latest-release.json",
            ),
            Source::mirror("ghproxy", "https://gh-proxy.com/", 10).with_index(
                "https://gh-proxy.com/https://raw.githubusercontent.com/astral-sh/python-build-standalone/latest-release/latest-release.json",
            ),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        source.index_url.clone()
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let triple = ctx.platform.llvm_triple();
        use std::collections::BTreeSet;
        let mut versions: BTreeSet<String> = BTreeSet::new();

        // Always include the latest catalog.
        let latest = self.catalog(ctx).await?;
        for a in &latest.assets {
            if Self::asset_is_install_only(&a.name, &triple) {
                if let Some(v) = Self::version_from_asset(&a.name) {
                    versions.insert(v);
                }
            }
        }

        // Best-effort: merge recent historical tags so older versions show up
        // too. Skipped silently if the GitHub API is rate-limited. Bounded to a
        // few recent tags to keep listing fast.
        let tags = self.list_release_tags(ctx, 6).await;
        for tag in tags.into_iter().take(6) {
            if tag == latest.tag {
                continue;
            }
            if let Ok(cat) = self.fetch_catalog_for_tag(ctx, &tag).await {
                for a in &cat.assets {
                    if Self::asset_is_install_only(&a.name, &triple) {
                        if let Some(v) = Self::version_from_asset(&a.name) {
                            versions.insert(v);
                        }
                    }
                }
            }
        }

        let mut out: Vec<VersionInfo> = versions
            .into_iter()
            .map(|v| VersionInfo {
                version: v,
                stable: true,
                lts: None,
            })
            .collect();
        out.sort_by(|a, b| cmp_versions(&a.version, &b.version));
        Ok(out)
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let triple = ctx.platform.llvm_triple();
        // Resolve a catalog (latest, or an older historical tag) that has this
        // version for the host triple. An explicit `-o tag=YYYYMMDD` pins the
        // PBS release tag (deterministic, no GitHub API needed).
        let catalog = match tv.options.get("tag") {
            Some(tag) => self.fetch_catalog_for_tag(ctx, tag).await?,
            None => self.catalog_with_version(ctx, &tv.version).await?,
        };

        // Find the asset matching this exact python version for the host triple.
        let asset = catalog
            .assets
            .iter()
            .find(|a| Self::asset_matches(&a.name, &tv.version, &triple))
            .ok_or_else(|| Error::VersionResolve {
                tool: self.id().to_string(),
                spec: tv.version.clone(),
                hint: Some(format!("no install_only asset for {triple}")),
            })?;

        // Full GitHub asset URL, then proxy-prefixed fallbacks for CN.
        let gh_url = format!(
            "{}/{}",
            catalog.asset_url_prefix.trim_end_matches('/'),
            asset.name
        );
        let mut urls = vec![gh_url.clone()];
        for s in &sources {
            // Skip the direct/official source (already added) and npm-style
            // registries; only real GitHub proxies help here.
            if s.download_url == "https://github.com/" {
                continue;
            }
            urls.push(http::join_url(
                s.download_url.trim_end_matches('/'),
                &gh_url,
            ));
        }

        let kind = ArchiveKind::from_name(&asset.name)?;
        // Checksum comes straight from SHA256SUMS — no extra request.
        let checksum = if asset.sha256.len() == 64 {
            Some(Checksum {
                algo: HashAlgo::Sha256,
                hex: asset.sha256.clone(),
            })
        } else {
            None
        };

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: tv.version.clone(),
            urls,
            file_name: asset.name.clone(),
            kind,
            checksum,
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
    /// Resolve the release catalog (tag + prefix + asset list w/ sha256),
    /// preferring a fresh 24h cache, then the network, then a stale cache.
    async fn catalog(&self, ctx: &Ctx) -> Result<Catalog> {
        let cache_file = ctx.dirs.remote_cache().join("python-catalog.json");
        const TTL_SECS: u64 = 24 * 3600;

        // 1. Fresh cache.
        if let Some(age) = file_age_secs(&cache_file) {
            if age <= TTL_SECS {
                if let Some(cat) = read_catalog(&cache_file) {
                    tracing::debug!("using cached python catalog");
                    return Ok(cat);
                }
            }
        }

        // 2. Network: metadata JSON -> SHA256SUMS at the prefix.
        match self.fetch_catalog(ctx).await {
            Ok(cat) => {
                if let Some(parent) = cache_file.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(bytes) = serde_json::to_vec_pretty(&cat) {
                    let _ = std::fs::write(&cache_file, bytes);
                }
                Ok(cat)
            }
            Err(e) => {
                // 3. Stale cache fallback.
                if let Some(cat) = read_catalog(&cache_file) {
                    tracing::warn!("{}", crate::i18n::tr("log.stale_python_cache"));
                    Ok(cat)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Fetch the metadata JSON, then its `SHA256SUMS`, trying each source in
    /// ranked order (failover).
    async fn fetch_catalog(&self, ctx: &Ctx) -> Result<Catalog> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut last_err: Option<Error> = None;
        for source in &sources {
            let index_url = match &source.index_url {
                Some(u) => u.clone(),
                None => continue,
            };
            let meta: LatestRelease = match http::get_json(&ctx.client, &index_url).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(source = %source.id, "{}", crate::i18n::trf("log.pbs_metadata_failed", &[("err", &e.to_string())]));
                    last_err = Some(e);
                    continue;
                }
            };
            // SHA256SUMS lives at the asset prefix; proxy it through the same
            // source host if this source is a proxy.
            let sums_url = format!("{}/SHA256SUMS", meta.asset_url_prefix.trim_end_matches('/'));
            let sums_url = if source.download_url != "https://github.com/" {
                http::join_url(source.download_url.trim_end_matches('/'), &sums_url)
            } else {
                sums_url
            };
            let body = match http::get_text(&ctx.client, &sums_url).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(source = %source.id, "{}", crate::i18n::trf("log.pbs_sha256sums_failed", &[("err", &e.to_string())]));
                    last_err = Some(e);
                    continue;
                }
            };
            let assets = parse_sha256sums(&body);
            if assets.is_empty() {
                last_err = Some(Error::other("empty SHA256SUMS"));
                continue;
            }
            return Ok(Catalog {
                tag: meta.tag,
                asset_url_prefix: meta.asset_url_prefix,
                assets,
            });
        }
        Err(last_err.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id().to_string(),
            tried: sources.len(),
        }))
    }

    /// Build the SHA256SUMS URL for a specific dated tag, proxying through a
    /// GitHub-proxy source when available (CN reachability).
    fn sums_url_for_tag(&self, ctx: &Ctx, tag: &str) -> Vec<String> {
        let base = format!(
            "https://github.com/astral-sh/python-build-standalone/releases/download/{tag}/SHA256SUMS"
        );
        // direct, then any proxy source
        let mut urls = vec![base.clone()];
        urls.push(format!("https://gh-proxy.com/{base}"));
        let _ = ctx;
        urls
    }

    /// Fetch the catalog for a specific historical tag by reading its
    /// SHA256SUMS (each dated release has its own).
    async fn fetch_catalog_for_tag(&self, ctx: &Ctx, tag: &str) -> Result<Catalog> {
        let prefix =
            format!("https://github.com/astral-sh/python-build-standalone/releases/download/{tag}");
        let mut last_err: Option<Error> = None;
        for url in self.sums_url_for_tag(ctx, tag) {
            match http::get_text(&ctx.client, &url).await {
                Ok(body) => {
                    let assets = parse_sha256sums(&body);
                    if !assets.is_empty() {
                        return Ok(Catalog {
                            tag: tag.to_string(),
                            asset_url_prefix: prefix,
                            assets,
                        });
                    }
                    last_err = Some(Error::other("empty SHA256SUMS"));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::other(format!("no SHA256SUMS for tag {tag}"))))
    }

    /// List recent PBS release tags (dated, e.g. "20260814") via the GitHub
    /// releases API. Token-aware; returns newest-first. Empty on rate-limit.
    async fn list_release_tags(&self, ctx: &Ctx, max: usize) -> Vec<String> {
        #[derive(serde::Deserialize)]
        struct Rel {
            tag_name: String,
            #[serde(default)]
            draft: bool,
        }
        let url = format!(
            "https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page={}",
            max.min(100)
        );
        match http::get_github_json::<Vec<Rel>>(&ctx.client, &url).await {
            Ok(rels) => rels
                .into_iter()
                .filter(|r| !r.draft && !r.tag_name.is_empty())
                .map(|r| r.tag_name)
                .collect(),
            Err(e) => {
                tracing::debug!("pbs tag list unavailable (rate limit?): {e}");
                Vec::new()
            }
        }
    }

    /// Resolve a Catalog that contains `version` for the host triple: the latest
    /// catalog if it has it, otherwise scan recent historical tags.
    async fn catalog_with_version(&self, ctx: &Ctx, version: &str) -> Result<Catalog> {
        let triple = ctx.platform.llvm_triple();
        let latest = self.catalog(ctx).await?;
        if latest
            .assets
            .iter()
            .any(|a| Self::asset_matches(&a.name, version, &triple))
        {
            return Ok(latest);
        }
        // Older version: scan recent tags (bounded) for one that has it.
        for tag in self.list_release_tags(ctx, 30).await {
            if tag == latest.tag {
                continue;
            }
            if let Ok(cat) = self.fetch_catalog_for_tag(ctx, &tag).await {
                if cat
                    .assets
                    .iter()
                    .any(|a| Self::asset_matches(&a.name, version, &triple))
                {
                    return Ok(cat);
                }
            }
        }
        Err(Error::VersionResolve {
            tool: self.id().to_string(),
            spec: version.to_string(),
            hint: Some(format!(
                "no install_only asset for {triple} in recent releases"
            )),
        })
    }
}

/// Parse a `SHA256SUMS` body: each line is `<hex>  <filename>`.
fn parse_sha256sums(body: &str) -> Vec<Asset> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let hash = match it.next() {
            Some(h) => h,
            None => continue,
        };
        let name = match it.next() {
            Some(n) => n.trim_start_matches('*'),
            None => continue,
        };
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            out.push(Asset {
                name: name.to_string(),
                sha256: hash.to_string(),
            });
        }
    }
    out
}

fn read_catalog(path: &std::path::Path) -> Option<Catalog> {
    let bytes = std::fs::read(path).ok()?;
    let cat: Catalog = serde_json::from_slice(&bytes).ok()?;
    if cat.assets.is_empty() {
        None
    } else {
        Some(cat)
    }
}

/// Compare two dotted numeric versions ascending.
pub fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let pb: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
    pa.cmp(&pb)
}

/// Age of a file in seconds since last modification, or None if unavailable.
fn file_age_secs(path: &std::path::Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    modified.elapsed().ok().map(|d| d.as_secs())
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
        let stripped =
            "cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz";
        assert!(!PythonBackend::asset_matches(
            stripped,
            "3.12.7",
            "x86_64-unknown-linux-gnu"
        ));
        // free-threaded variant excluded
        let ft =
            "cpython-3.13.1+20241016-x86_64-unknown-linux-gnu-freethreaded-install_only.tar.gz";
        assert!(!PythonBackend::asset_matches(
            ft,
            "3.13.1",
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
    fn parse_sums_extracts_assets() {
        let body = "\
391e2bbe4da892fd7dd9f773f42ad8eae82f33d3d4fc8f0025af80b4dfa134b3  cpython-3.10.21+20260814-x86_64-unknown-linux-gnu-install_only.tar.gz
3297691ae34f75fed81ac424e040145fccb0bafe8e581cd5cadbddfa1c0766c0  cpython-3.12.14+20260814-x86_64-unknown-linux-gnu-install_only.tar.gz
not-a-hash  garbage-line
";
        let assets = parse_sha256sums(body);
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0].sha256.len(), 64);
        assert!(assets[1].name.contains("3.12.14"));
    }

    #[test]
    fn version_ordering() {
        assert_eq!(cmp_versions("3.9.1", "3.12.0"), std::cmp::Ordering::Less);
        assert_eq!(cmp_versions("3.12.7", "3.12.7"), std::cmp::Ordering::Equal);
    }
}
