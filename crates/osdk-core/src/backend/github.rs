//! Generic GitHub-release backend, addressed as `github:owner/repo`.
//!
//! Downloads a release asset matching the host platform and installs it. Two
//! asset shapes are handled: archives (tar.*/zip → extracted) and bare binaries
//! (installed directly into `bin/`). Version specs map to release tags
//! (`latest` → the latest release). Mirrors: a CN GitHub proxy fallback.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::dirs::InstallLocator;
use crate::error::{Error, Result};
use crate::http;
use crate::inventory::{DynamicToolBin, DynamicToolManifest};
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Os};
use crate::source::Source;
use crate::tool::{InstallIdentity, InstallScope};
use crate::verification::GithubAttestation;
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

#[derive(Debug, Clone)]
struct SelectedArtifact {
    asset: GhAsset,
    urls: Vec<String>,
    checksum: Option<pipeline::Checksum>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StaticCatalog {
    schema: u32,
    releases: Vec<StaticRelease>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StaticRelease {
    tag: String,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<StaticAsset>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StaticAsset {
    name: String,
    url: String,
    checksum: String,
    os: String,
    arch: String,
    #[serde(default)]
    libc: Option<String>,
}

#[derive(Debug, Clone)]
struct AssetRules {
    regex: Option<regex::Regex>,
    template: Option<String>,
    bins: Vec<PathBuf>,
    rename: Option<String>,
    strip_components: usize,
    os: Option<String>,
    arch: Option<String>,
    libc: Option<String>,
}

impl GithubBackend {
    /// Parse a `github:owner/repo` id into a backend. Returns None if the id
    /// doesn't carry a valid owner/repo.
    pub fn from_id(id: &str) -> Option<GithubBackend> {
        let id = crate::inventory::canonical_dynamic_id(id).ok()?;
        let rest = id.strip_prefix("github:")?;
        let mut components = rest.split('/');
        let owner = components.next()?;
        let repo = components.next()?;
        if components.next().is_some() {
            return None;
        }
        let owner = owner.trim().to_string();
        let repo = repo.trim().trim_end_matches(".git").to_string();
        if !valid_repository_component(&owner) || !valid_repository_component(&repo) {
            return None;
        }
        Some(GithubBackend { id, owner, repo })
    }

    fn releases_api(&self, page: usize) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/releases?per_page=100&page={page}",
            self.owner, self.repo,
        )
    }

    fn release_api(&self, tag: &str) -> Result<String> {
        let mut url = reqwest::Url::parse(&format!(
            "https://api.github.com/repos/{}/{}/releases/tags/",
            self.owner, self.repo
        ))
        .map_err(|error| Error::other(format!("invalid GitHub release URL: {error}")))?;
        url.path_segments_mut()
            .map_err(|_| Error::other("invalid GitHub release URL"))?
            .push(tag);
        Ok(url.into())
    }

    fn releases_atom(&self) -> String {
        format!(
            "https://github.com/{}/{}/releases.atom",
            self.owner, self.repo
        )
    }

    fn expanded_assets(&self, tag: &str) -> Result<String> {
        let mut url = reqwest::Url::parse(&format!(
            "https://github.com/{}/{}/releases/expanded_assets/",
            self.owner, self.repo
        ))
        .map_err(|error| Error::other(format!("invalid GitHub assets URL: {error}")))?;
        url.path_segments_mut()
            .map_err(|_| Error::other("invalid GitHub assets URL"))?
            .push(tag);
        Ok(url.into())
    }

    fn attestation(&self, ctx: &Ctx, sources: &[Source]) -> Option<GithubAttestation> {
        let policy = ctx.config.settings.attestations;
        (policy != crate::config::AttestationPolicy::Off).then(|| GithubAttestation {
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            policy,
            sources: sources.to_vec(),
        })
    }

    async fn api_releases(&self, ctx: &Ctx, sources: &[Source]) -> Result<Vec<GhRelease>> {
        let mut releases = Vec::new();
        for page in 1..=10 {
            let api = self.releases_api(page);
            let urls = http::github_url_candidates(sources, &api);
            let page_releases: Vec<GhRelease> =
                http::get_cached_github_json_from_urls(ctx, &api, &urls).await?;
            let done = page_releases.len() < 100;
            releases.extend(page_releases);
            if done {
                break;
            }
        }
        Ok(releases)
    }

    async fn releases(&self, ctx: &Ctx, sources: &[Source]) -> Result<Vec<GhRelease>> {
        match self.api_releases(ctx, sources).await {
            Ok(releases) => Ok(releases),
            Err(api_error) if api_error.is_anonymous_github_rate_limit() => {
                match self.public_releases(ctx, sources).await {
                    Ok(releases) if !releases.is_empty() => {
                        tracing::warn!(
                            repository = %format!("{}/{}", self.owner, self.repo),
                            "{}",
                            crate::i18n::tr("log.github_public_fallback")
                        );
                        Ok(releases)
                    }
                    Ok(_) => Err(api_error),
                    Err(web_error) => {
                        tracing::debug!(error = %web_error, "public GitHub releases fallback failed");
                        Err(api_error)
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn release_for_tag(
        &self,
        ctx: &Ctx,
        sources: &[Source],
        version: &str,
    ) -> Result<GhRelease> {
        let tags = tag_candidates(version);
        let mut rate_limit_error = None;
        for tag in &tags {
            let api = self.release_api(tag)?;
            let urls = http::github_url_candidates(sources, &api);
            match http::get_cached_github_json_from_urls(ctx, &api, &urls).await {
                Ok(release) => return Ok(release),
                Err(error) if ctx.config.settings.offline => {
                    return self.release_for_tag_from_list_cache(ctx, version, error);
                }
                Err(error) if error.is_anonymous_github_rate_limit() => {
                    rate_limit_error = Some(error);
                    break;
                }
                Err(error) if error.status() == Some(404) => continue,
                Err(error) => return Err(error),
            }
        }

        let Some(api_error) = rate_limit_error else {
            return Err(Error::VersionResolve {
                tool: self.id().to_string(),
                spec: version.to_string(),
                hint: Some("release tag not found".into()),
            });
        };
        for tag in &tags {
            match self.public_assets(ctx, sources, tag).await {
                Ok(assets) if !assets.is_empty() => {
                    tracing::warn!(
                        repository = %format!("{}/{}", self.owner, self.repo),
                        tag,
                        "{}",
                        crate::i18n::tr("log.github_public_fallback")
                    );
                    return Ok(GhRelease {
                        tag_name: tag.clone(),
                        draft: false,
                        prerelease: crate::backend::python::is_prerelease(
                            tag.trim_start_matches('v'),
                        ),
                        assets,
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(tag, error = %error, "public GitHub assets fallback failed");
                }
            }
        }
        Err(api_error)
    }

    fn release_for_tag_from_list_cache(
        &self,
        ctx: &Ctx,
        version: &str,
        exact_cache_error: Error,
    ) -> Result<GhRelease> {
        for page in 1..=10 {
            let api = self.releases_api(page);
            let cache = http::metadata_cache_path(ctx, &api);
            let Ok(bytes) = std::fs::read(&cache) else {
                break;
            };
            let page_releases: Vec<GhRelease> = serde_json::from_slice(&bytes)?;
            let done = page_releases.len() < 100;
            if let Some(release) = page_releases.into_iter().find(|release| {
                release.tag_name.trim_start_matches('v') == version.trim_start_matches('v')
            }) {
                return Ok(release);
            }
            if done {
                break;
            }
        }
        Err(exact_cache_error)
    }

    async fn public_releases(&self, ctx: &Ctx, sources: &[Source]) -> Result<Vec<GhRelease>> {
        let canonical = self.releases_atom();
        let urls = http::github_url_candidates(sources, &canonical);
        let atom = http::get_cached_text_from_urls(ctx, &canonical, &urls, |text| {
            !parse_release_tags(text, &self.owner, &self.repo).is_empty()
        })
        .await?;
        let tags = parse_release_tags(&atom, &self.owner, &self.repo);
        Ok(tags
            .into_iter()
            .map(|tag_name| GhRelease {
                prerelease: crate::backend::python::is_prerelease(tag_name.trim_start_matches('v')),
                tag_name,
                draft: false,
                assets: Vec::new(),
            })
            .collect())
    }

    async fn public_assets(
        &self,
        ctx: &Ctx,
        sources: &[Source],
        tag: &str,
    ) -> Result<Vec<GhAsset>> {
        let canonical = self.expanded_assets(tag)?;
        let urls = http::github_url_candidates(sources, &canonical);
        let html = http::get_cached_text_from_urls(ctx, &canonical, &urls, |text| {
            !parse_release_assets(text, &self.owner, &self.repo, tag).is_empty()
        })
        .await?;
        Ok(parse_release_assets(&html, &self.owner, &self.repo, tag))
    }

    fn rules(options: &std::collections::BTreeMap<String, String>) -> Result<AssetRules> {
        if options.contains_key("asset-regex") && options.contains_key("asset-template") {
            return Err(Error::config(
                "asset-regex and asset-template are mutually exclusive",
            ));
        }
        if options.contains_key("bin") && options.contains_key("bins") {
            return Err(Error::config("bin and bins are mutually exclusive"));
        }
        let regex = options
            .get("asset-regex")
            .map(|value| {
                regex::Regex::new(value)
                    .map_err(|error| Error::config(format!("invalid asset-regex: {error}")))
            })
            .transpose()?;
        let bins = options
            .get("bins")
            .or_else(|| options.get("bin"))
            .map(|value| {
                value
                    .split(',')
                    .filter(|item| !item.trim().is_empty())
                    .map(|item| {
                        let path = PathBuf::from(item.trim());
                        if path.is_absolute()
                            || path.components().any(|component| {
                                !matches!(component, std::path::Component::Normal(_))
                            })
                        {
                            return Err(Error::config(format!(
                                "unsafe GitHub bin path `{}`",
                                path.display()
                            )));
                        }
                        Ok(path)
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        let strip_components = options
            .get("strip-components")
            .map(|value| {
                value.parse::<usize>().map_err(|error| {
                    Error::config(format!("invalid strip-components `{value}`: {error}"))
                })
            })
            .transpose()?
            .unwrap_or_default();
        let os = options.get("os").cloned();
        let arch = options.get("arch").cloned();
        let libc = options.get("libc").cloned();
        if os
            .as_deref()
            .is_some_and(|value| !matches!(value, "linux" | "macos" | "darwin" | "windows"))
        {
            return Err(Error::config("invalid GitHub target os"));
        }
        if arch.as_deref().is_some_and(|value| {
            !matches!(
                value,
                "x64" | "x86_64" | "amd64" | "arm64" | "aarch64" | "x86" | "i686" | "arm" | "armv7"
            )
        }) {
            return Err(Error::config("invalid GitHub target arch"));
        }
        if libc
            .as_deref()
            .is_some_and(|value| !matches!(value, "gnu" | "musl" | "none"))
        {
            return Err(Error::config("invalid GitHub target libc"));
        }
        let rename = options.get("rename").cloned();
        if let Some(name) = rename.as_deref() {
            pipeline::validate_safe_filename("GitHub executable rename", name)?;
        }
        Ok(AssetRules {
            regex,
            template: options.get("asset-template").cloned(),
            bins,
            rename,
            strip_components,
            os,
            arch,
            libc,
        })
    }

    async fn static_catalog(
        &self,
        ctx: &Ctx,
        options: &std::collections::BTreeMap<String, String>,
    ) -> Result<Option<StaticCatalog>> {
        let Some(source) = options.get("catalog-url") else {
            return Ok(None);
        };
        let expected = options
            .get("catalog-sha256")
            .ok_or_else(|| Error::config("catalog-sha256 is required with catalog-url"))?;
        let bytes = if source.starts_with("http://") || source.starts_with("https://") {
            if ctx.config.settings.offline {
                let cache = static_catalog_cache(ctx, expected);
                std::fs::read(&cache).map_err(|_| {
                    Error::other(format!(
                        "offline static GitHub catalog cache miss for {source}"
                    ))
                })?
            } else {
                let bytes = ctx
                    .client
                    .get(source)
                    .send()
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?
                    .to_vec();
                verify_catalog_bytes(source, expected, &bytes)?;
                write_atomic(&static_catalog_cache(ctx, expected), &bytes)?;
                bytes
            }
        } else {
            let path = source.strip_prefix("file://").unwrap_or(source);
            std::fs::read(path).map_err(|error| Error::io(path, error))?
        };
        verify_catalog_bytes(source, expected, &bytes)?;
        let catalog: StaticCatalog = serde_json::from_slice(&bytes)?;
        if catalog.schema != 1 {
            return Err(Error::config(format!(
                "unsupported GitHub static catalog schema {}",
                catalog.schema
            )));
        }
        if catalog.releases.is_empty() {
            return Err(Error::config("GitHub static catalog contains no releases"));
        }
        for release in &catalog.releases {
            if release.tag.trim().is_empty() || release.assets.is_empty() {
                return Err(Error::config(
                    "GitHub static catalog release requires a tag and assets",
                ));
            }
            for asset in &release.assets {
                if asset.name.trim().is_empty()
                    || !static_asset_url_is_persistable(&asset.url)
                    || asset.os.trim().is_empty()
                    || asset.arch.trim().is_empty()
                {
                    return Err(Error::config(format!(
                        "invalid GitHub static catalog asset in {}",
                        release.tag
                    )));
                }
                pipeline::parse_checksum(&asset.checksum)?;
            }
        }
        Ok(Some(catalog))
    }

    async fn select_install_artifact(
        &self,
        ctx: &Ctx,
        tv: &ToolVersion,
        rules: &AssetRules,
        sources: &[Source],
    ) -> Result<SelectedArtifact> {
        if let Some(artifact) = pipeline::locked_artifact(tv)? {
            let checksum = artifact
                .checksum
                .as_deref()
                .map(pipeline::parse_checksum)
                .transpose()?;
            return Ok(SelectedArtifact {
                urls: http::github_url_candidates(sources, &artifact.url),
                asset: GhAsset {
                    name: artifact.file_name,
                    browser_download_url: artifact.url,
                },
                checksum,
            });
        }

        let want = tv.version.trim_start_matches('v');
        let (assets, static_checksums) =
            if let Some(catalog) = self.static_catalog(ctx, &tv.options).await? {
                let release = catalog
                    .releases
                    .into_iter()
                    .find(|release| release.tag.trim_start_matches('v') == want)
                    .ok_or_else(|| Error::VersionResolve {
                        tool: self.id().into(),
                        spec: tv.version.clone(),
                        hint: Some("static catalog tag not found".into()),
                    })?;
                let matching: Vec<_> = release
                    .assets
                    .into_iter()
                    .filter(|asset| static_asset_matches(asset, ctx, rules))
                    .collect();
                (
                    matching
                        .iter()
                        .map(|asset| GhAsset {
                            name: asset.name.clone(),
                            browser_download_url: asset.url.clone(),
                        })
                        .collect(),
                    matching
                        .into_iter()
                        .map(|asset| (asset.name, asset.checksum))
                        .collect::<BTreeMap<_, _>>(),
                )
            } else {
                let release = self.release_for_tag(ctx, sources, &tv.version).await?;
                (release.assets, BTreeMap::new())
            };

        let asset = select_asset(self, &assets, &tv.version, ctx, rules)?;
        let urls = http::github_url_candidates(sources, &asset.browser_download_url);
        let mut checksum = static_checksums
            .get(&asset.name)
            .map(|value| pipeline::parse_checksum(value))
            .transpose()?;

        // Checksum discovery, strongest first:
        // 1. a minisign-signed checksums manifest (trusted key);
        // 2. per-asset sidecar / unsigned shared manifest.
        if ctx.config.settings.verify_signatures && !ctx.config.settings.offline {
            for url in &urls {
                let dir = url
                    .rsplit_once('/')
                    .map(|(directory, _)| directory)
                    .unwrap_or("");
                match pipeline::verify::signed_manifest_checksum(
                    &ctx.client,
                    &self.id,
                    dir,
                    &asset.name,
                )
                .await
                {
                    Ok(Some(found)) => {
                        tracing::info!(source = %self.id, "{}", crate::i18n::tr("log.signature_verified"));
                        checksum = Some(found);
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if checksum.is_none() && !ctx.config.settings.offline {
            for url in &urls {
                if let Some(found) =
                    pipeline::verify::discover_asset_checksum(&ctx.client, url).await
                {
                    checksum = Some(found);
                    break;
                }
            }
        }

        Ok(SelectedArtifact {
            asset,
            urls,
            checksum,
        })
    }

    /// Score how well an asset name matches the host platform. Higher is better;
    /// None means it clearly doesn't match (wrong os/arch).
    fn score_asset(&self, name: &str, ctx: &Ctx, rules: Option<&AssetRules>) -> Option<i32> {
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

        let target_os = rules
            .and_then(|rules| rules.os.as_deref())
            .unwrap_or_else(|| os_token(ctx));
        let os_ok = match target_os {
            "linux" => n.contains("linux"),
            "macos" | "darwin" => {
                n.contains("darwin")
                    || n.contains("macos")
                    || n.contains("apple")
                    || n.contains("osx")
            }
            "windows" | "win" => n.contains("windows") || n.contains("win") || n.ends_with(".exe"),
            _ => false,
        };
        // Some assets omit OS (bare binaries); allow but score lower.
        let mut score = 0;
        if os_ok {
            score += 10;
        } else if mentions_other_os_token(&n, target_os) {
            return None; // explicitly a different OS
        }

        let target_arch = rules
            .and_then(|rules| rules.arch.as_deref())
            .unwrap_or_else(|| arch_token(ctx));
        let arch_ok = match target_arch {
            "x64" | "x86_64" | "amd64" => {
                n.contains("x86_64") || n.contains("amd64") || n.contains("x64")
            }
            "arm64" | "aarch64" => n.contains("aarch64") || n.contains("arm64"),
            "x86" | "i686" => n.contains("i686") || n.contains("i386") || n.contains("x86"),
            "arm" | "armv7" => n.contains("armv7") || n.contains("armhf") || n.contains("arm"),
            _ => false,
        };
        if arch_ok {
            score += 10;
        } else if mentions_other_arch_token(&n, target_arch) {
            return None;
        }

        // Prefer archives we can extract; then musl/gnu preferences on linux.
        if ArchiveKind::from_name(&n).is_ok() {
            score += 3;
        }
        if target_os == "linux" {
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
            Source::mirror("ghproxy", "https://gh-proxy.com/https://github.com/", 10)
                .with_index("https://gh-proxy.com/https://api.github.com/"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, _source: &Source) -> Option<String> {
        // Probing the API is rate-limited; skip (selection falls back to order).
        None
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let releases = self.releases(ctx, &sources).await?;
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
        crate::backend::dynamic::validate_options(self.id(), &req.options)?;
        let prerelease_request = match &req.spec {
            VersionSpec::Exact(version) => crate::backend::python::is_prerelease(version),
            VersionSpec::Prefix(channel) => {
                matches!(channel.as_str(), "canary" | "nightly" | "beta")
            }
            _ => false,
        };
        if prerelease_request
            && matches!(
                ctx.config.settings.prerelease,
                crate::config::PrereleasePolicy::Never
            )
        {
            return Err(Error::VersionResolve {
                tool: self.id().into(),
                spec: req.spec.to_string(),
                hint: Some("pre-release versions are disabled".into()),
            });
        }
        if req
            .options
            .contains_key(pipeline::LOCKED_ARTIFACT_URL_OPTION)
        {
            if let VersionSpec::Exact(version) = &req.spec {
                let mut resolved = ToolVersion::new(self.id(), version);
                resolved.options = req.options.clone();
                return Ok(resolved);
            }
        }
        if let Some(catalog) = self.static_catalog(ctx, &req.options).await? {
            let versions = static_versions(&catalog);
            let selected = match &req.spec {
                VersionSpec::Exact(version) => versions
                    .iter()
                    .find(|candidate| candidate.version == *version),
                VersionSpec::Prefix(channel)
                    if matches!(channel.as_str(), "canary" | "nightly" | "beta") =>
                {
                    versions.iter().rev().find(|candidate| {
                        !candidate.stable
                            && candidate.version.to_ascii_lowercase().contains(channel)
                    })
                }
                _ => crate::version::select_version_with_prerelease(
                    &req.spec,
                    &versions,
                    ctx.config.settings.prerelease,
                ),
            }
            .ok_or_else(|| Error::VersionResolve {
                tool: self.id().into(),
                spec: req.spec.to_string(),
                hint: Some("no matching static catalog release".into()),
            })?;
            let mut resolved = ToolVersion::new(self.id(), &selected.version);
            resolved.options = req.options.clone();
            let rules = Self::rules(&resolved.options)?;
            let release = catalog
                .releases
                .iter()
                .find(|release| {
                    release.tag.trim_start_matches('v') == selected.version.trim_start_matches('v')
                })
                .expect("selected version originated from catalog");
            let assets = release
                .assets
                .iter()
                .filter(|asset| static_asset_matches(asset, ctx, &rules))
                .map(|asset| GhAsset {
                    name: asset.name.clone(),
                    browser_download_url: asset.url.clone(),
                })
                .collect::<Vec<_>>();
            let asset = select_asset(self, &assets, &selected.version, ctx, &rules)?;
            let static_asset = release
                .assets
                .iter()
                .find(|candidate| candidate.name == asset.name)
                .expect("selected asset originated from catalog");
            resolved.options.insert(
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                static_asset.url.clone(),
            );
            resolved.options.insert(
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                static_asset.name.clone(),
            );
            resolved.options.insert(
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                static_asset.checksum.clone(),
            );
            return Ok(resolved);
        }
        // For github, an exact tag passes through; otherwise resolve against the
        // release list (latest/prefix).
        if let VersionSpec::Exact(v) = &req.spec {
            if crate::backend::python::is_prerelease(v)
                && matches!(
                    ctx.config.settings.prerelease,
                    crate::config::PrereleasePolicy::Never
                )
            {
                return Err(Error::VersionResolve {
                    tool: self.id().into(),
                    spec: v.clone(),
                    hint: Some("pre-release versions are disabled".into()),
                });
            }
            let mut tv = ToolVersion::new(self.id(), v.clone());
            tv.options = req.options.clone();
            return Ok(tv);
        }
        let versions = self.list_remote_versions(ctx).await?;
        let chosen = match &req.spec {
            VersionSpec::Prefix(channel)
                if matches!(channel.as_str(), "canary" | "nightly" | "beta") =>
            {
                versions.iter().rev().find(|candidate| {
                    !candidate.stable && candidate.version.to_ascii_lowercase().contains(channel)
                })
            }
            _ => crate::version::select_version_with_prerelease(
                &req.spec,
                &versions,
                ctx.config.settings.prerelease,
            ),
        }
        .ok_or_else(|| Error::VersionResolve {
            tool: self.id().to_string(),
            spec: req.spec.to_string(),
            hint: Some("no matching release under prerelease policy".into()),
        })?;
        let mut tv = ToolVersion::new(self.id(), chosen.version.clone());
        tv.options = req.options.clone();
        Ok(tv)
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        crate::backend::dynamic::validate_options(self.id(), &tv.options)?;
        let rules = Self::rules(&tv.options)?;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let attestation = self.attestation(ctx, &sources);
        let selected = self
            .select_install_artifact(ctx, tv, &rules, &sources)
            .await?;
        let locator = github_install_locator_for_artifact(
            ctx,
            self.id(),
            tv,
            &selected.asset,
            selected.checksum.as_ref(),
        )?;
        // The shared archive pipeline only keeps its install lock while it is
        // materializing files. GitHub-specific post-processing and inventory
        // publication happen afterwards, and the bare-binary path does not use
        // that lock at all. Keep a separate outer lock across the whole
        // operation so two direct backend callers cannot race and bind the
        // winner's bytes to the loser's option identity.
        let _identity_lock = acquire_github_install_lock(&locator).await?;
        if validate_complete_install_identity(ctx, self, tv, &locator)? {
            return Ok(());
        }
        // Archive vs bare binary.
        match ArchiveKind::from_name(&selected.asset.name) {
            Ok(kind) => {
                let plan = InstallPlan {
                    tool: self.id().to_string(),
                    version: tv.version.clone(),
                    urls: selected.urls,
                    file_name: selected.asset.name,
                    kind,
                    checksum: selected.checksum,
                    // Some archives have a top dir, some don't; strip only when a
                    // single root dir is present (extract handles the no-op).
                    strip_root: rules.strip_components == 0,
                    subdir: tv
                        .options
                        .get(pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION)
                        .or_else(|| tv.options.get("catalog-subdir"))
                        .map(PathBuf::from),
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
                pipeline::run_with_attestation_unfinalized_at(
                    &plan,
                    &pctx,
                    attestation.as_ref(),
                    &locator,
                )
                .await?;
                postprocess_archive(ctx, &locator, &rules)?;
            }
            Err(_) => {
                // Treat as a bare executable named after the repo.
                let exe_name = normalize_executable_name(
                    rules.rename.as_deref().unwrap_or(&self.repo),
                    ctx.platform.os,
                );
                pipeline::install_single_binary_unfinalized_at(
                    &ctx.client,
                    &ctx.dirs,
                    &locator,
                    &selected.urls,
                    exe_name.trim_end_matches(ctx.platform.os.exe_suffix()),
                    &selected.asset.name,
                    ctx.platform.os,
                    selected.checksum.as_ref(),
                    ctx.show_progress,
                    ctx.config.settings.offline,
                    ctx.config.settings.require_checksums,
                    attestation.as_ref(),
                )
                .await?;
            }
        }
        finalize_dynamic_install(&locator)?;
        Ok(())
    }

    async fn uninstall(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let locator = github_install_locator(ctx, self.id(), tv)?;
        let install_root = locator.install_root();
        match std::fs::symlink_metadata(install_root) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                std::fs::remove_dir_all(install_root)
                    .map_err(|error| Error::io(install_root, error))?;
            }
            Ok(_) => {
                std::fs::remove_file(install_root)
                    .map_err(|error| Error::io(install_root, error))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(install_root, error)),
        }
        Ok(())
    }

    fn list_installed(&self, ctx: &Ctx) -> Result<Vec<String>> {
        let report = crate::inventory::scan_installs(
            &ctx.dirs.installs,
            &crate::inventory::ScanOptions::default(),
        )?;
        Ok(report
            .installs
            .iter()
            .filter(|install| {
                let identity = &install.manifest.identity;
                identity.tool == self.id
                    && identity.platform == ctx.platform.to_string()
                    && identity.scope == InstallScope::Isolated
                    && github_install_candidate_is_valid(ctx, &install.install_root, identity)
                        .unwrap_or(false)
            })
            .map(|install| install.manifest.identity.version.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    fn ensure_post_install(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<()> {
        let locator = github_install_locator(ctx, self.id(), tv)?;
        if !validate_complete_install_identity(ctx, self, tv, &locator)? {
            return Err(Error::other(format!(
                "dynamic tool `{}@{}` is not completely installed",
                self.id(),
                tv.version
            )));
        }
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let root = github_install_locator(ctx, self.id(), tv)?
            .install_root()
            .to_path_buf();
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

pub(crate) fn github_install_locator(
    ctx: &Ctx,
    backend_id: &str,
    version: &ToolVersion,
) -> Result<InstallLocator> {
    if pipeline::locked_artifact(version)?.is_some() {
        return github_install_locator_for(&ctx.dirs, ctx.platform, backend_id, version);
    }
    github_installed_locator(ctx, backend_id, version)?.ok_or_else(|| {
        Error::other(format!(
            "GitHub install identity for `{backend_id}@{}` has no unique complete artifact candidate; reinstall it or use a lockfile with exact artifact identity",
            version.version
        ))
    })
}

/// Derive the exact locator for a GitHub request whose locked artifact
/// material is already present. This variant is used by lockfile serialization
/// where only directory and platform state are available.
pub fn github_install_locator_for(
    dirs: &crate::dirs::Dirs,
    platform: crate::platform::Platform,
    backend_id: &str,
    version: &ToolVersion,
) -> Result<InstallLocator> {
    if let Some(artifact) = pipeline::locked_artifact(version)? {
        return github_install_locator_for_artifact_parts(
            dirs,
            platform,
            backend_id,
            version,
            &GhAsset {
                name: artifact.file_name,
                browser_download_url: artifact.url,
            },
            artifact
                .checksum
                .as_deref()
                .map(pipeline::parse_checksum)
                .transpose()?
                .as_ref(),
        );
    }
    Err(Error::other(format!(
        "GitHub install identity for `{backend_id}@{}` requires locked artifact material",
        version.version
    )))
}

fn github_installed_locator(
    ctx: &Ctx,
    backend_id: &str,
    version: &ToolVersion,
) -> Result<Option<InstallLocator>> {
    let expected_options = crate::backend::dynamic::identity_options(backend_id, &version.options)?;
    let report = crate::inventory::scan_installs(
        &ctx.dirs.installs,
        &crate::inventory::ScanOptions::default(),
    )?;
    let mut candidates = report.installs.into_iter().filter(|install| {
        let identity = &install.manifest.identity;
        identity.tool == backend_id
            && identity.version == version.version
            && identity.platform == ctx.platform.to_string()
            && identity.scope == InstallScope::Isolated
            && identity.material_options == expected_options
            && github_install_candidate_is_valid(ctx, &install.install_root, identity)
                .unwrap_or(false)
    });
    let Some(first) = candidates.next() else {
        return Ok(None);
    };
    if candidates.next().is_some() {
        return Err(Error::other(format!(
            "GitHub install identity for `{backend_id}@{}` is ambiguous across multiple artifact variants; use a lockfile with exact artifact identity or uninstall the unwanted variant",
            version.version
        )));
    }
    InstallLocator::new(&ctx.dirs, first.manifest.identity).map(Some)
}

fn github_install_locator_for_artifact(
    ctx: &Ctx,
    backend_id: &str,
    version: &ToolVersion,
    asset: &GhAsset,
    checksum: Option<&pipeline::Checksum>,
) -> Result<InstallLocator> {
    github_install_locator_for_artifact_parts(
        &ctx.dirs,
        ctx.platform,
        backend_id,
        version,
        asset,
        checksum,
    )
}

fn github_install_locator_for_artifact_parts(
    dirs: &crate::dirs::Dirs,
    platform: crate::platform::Platform,
    backend_id: &str,
    version: &ToolVersion,
    asset: &GhAsset,
    checksum: Option<&pipeline::Checksum>,
) -> Result<InstallLocator> {
    let subdir = version
        .options
        .get(pipeline::LOCKED_ARTIFACT_SUBDIR_OPTION)
        .or_else(|| version.options.get("catalog-subdir"));
    let materials = github_artifact_materials(asset, checksum, subdir.map(String::as_str));
    let identity = InstallIdentity::new(
        backend_id,
        &version.version,
        platform.to_string(),
        InstallScope::Isolated,
        &version.options,
        Vec::new(),
        materials,
    )?;
    InstallLocator::new(dirs, identity)
}

fn github_artifact_materials(
    asset: &GhAsset,
    checksum: Option<&pipeline::Checksum>,
    subdir: Option<&str>,
) -> BTreeMap<String, String> {
    let mut materials = BTreeMap::from([("artifact-file".into(), asset.name.clone())]);
    if let Some(checksum) = checksum {
        materials.insert("artifact-checksum".into(), format_checksum(checksum));
    } else {
        materials.insert(
            "artifact-url-blake3".into(),
            github_artifact_url_hash(&asset.browser_download_url),
        );
    }
    if let Some(subdir) = subdir {
        materials.insert("artifact-subdir".into(), subdir.to_string());
    }
    materials
}

fn github_artifact_url_hash(url: &str) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("osdk-github-artifact-url-v1");
    hasher.update(url.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn format_checksum(checksum: &pipeline::Checksum) -> String {
    let algorithm = match checksum.algo {
        pipeline::HashAlgo::Sha256 => "sha256",
        pipeline::HashAlgo::Sha512 => "sha512",
        pipeline::HashAlgo::Blake3 => "blake3",
    };
    format!("{algorithm}:{}", checksum.hex.to_ascii_lowercase())
}

async fn acquire_github_install_lock(locator: &InstallLocator) -> Result<crate::lock::FileLock> {
    let path = locator.lock_path().to_path_buf();
    tokio::task::spawn_blocking(move || crate::lock::FileLock::acquire(path))
        .await
        .map_err(|error| Error::other(format!("GitHub install lock task failed: {error}")))?
}

/// Validate an already-complete install without ever upgrading or rewriting
/// its inventory. Returning `false` means no complete install exists and the
/// caller may perform a real installation. Any complete install that cannot
/// prove the requested schema-1 install identity is rejected fail-closed.
fn validate_complete_install_identity(
    ctx: &Ctx,
    backend: &GithubBackend,
    version: &ToolVersion,
    locator: &InstallLocator,
) -> Result<bool> {
    let install_root = locator.install_root().to_path_buf();
    if !is_regular_file(&install_root.join(".osdk-complete")) {
        return Ok(false);
    }
    match github_install_candidate_is_valid(ctx, &install_root, locator.identity()) {
        Ok(true) => {}
        Ok(false) => {
            return Err(Error::other(format!(
                "refusing to reuse complete dynamic tool `{}@{}` with missing, legacy, or invalid install identity; uninstall and reinstall it",
                backend.id(), version.version
            )));
        }
        Err(error) => {
            return Err(Error::other(format!(
                "refusing to reuse complete dynamic tool `{}@{}` installed with a different identity or invalid receipt; uninstall and reinstall it: {error}",
                backend.id(), version.version
            )));
        }
    }
    Ok(true)
}

/// Validate a scanned GitHub candidate without consulting the network. This is
/// shared by lifecycle reuse and restart-time config/activation recovery.
pub fn github_install_candidate_is_valid(
    ctx: &Ctx,
    install_root: &Path,
    identity: &InstallIdentity,
) -> Result<bool> {
    github_install_candidate_is_valid_for_dirs(&ctx.dirs, install_root, identity)
}

/// Validate a persisted GitHub candidate when callers only have storage and
/// platform state (for example lockfile serialization after installation).
pub fn github_install_candidate_is_valid_for_dirs(
    dirs: &crate::dirs::Dirs,
    install_root: &Path,
    identity: &InstallIdentity,
) -> Result<bool> {
    if identity.scope != InstallScope::Isolated
        || !is_regular_file(&install_root.join(".osdk-complete"))
        || !is_regular_file(&DynamicToolManifest::manifest_path(install_root))
        || !is_regular_file(&install_root.join(".osdk-artifact.json"))
    {
        return Ok(false);
    }
    let locator = InstallLocator::new(dirs, identity.clone())?;
    if !locator.validates_existing_install_root(install_root) {
        return Ok(false);
    }
    let manifest = match DynamicToolManifest::load(install_root) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    if !manifest.matches_identity(identity) {
        return Err(Error::other(format!(
            "GitHub install identity mismatch at {}",
            DynamicToolManifest::manifest_path(install_root).display()
        )));
    }
    let Some(receipt) = pipeline::artifact_receipt_at(install_root) else {
        return Ok(false);
    };
    if !github_receipt_matches_identity(&receipt, identity) {
        return Err(Error::other(format!(
            "GitHub artifact receipt does not match install identity at {}",
            install_root.display()
        )));
    }
    let canonical_root =
        dunce::canonicalize(install_root).map_err(|error| Error::io(install_root, error))?;
    for bin in &manifest.bins {
        let path = install_root.join(&bin.path);
        let canonical = dunce::canonicalize(&path).map_err(|error| Error::io(&path, error))?;
        if !canonical.is_file() || !canonical.starts_with(&canonical_root) {
            return Err(Error::other(format!(
                "GitHub inventory bin `{}` does not resolve inside {}",
                bin.name,
                install_root.display()
            )));
        }
    }
    Ok(true)
}

fn github_receipt_matches_identity(
    receipt: &pipeline::ArtifactReceipt,
    identity: &InstallIdentity,
) -> bool {
    let Some(expected_file) = identity.materials.get("artifact-file") else {
        return false;
    };
    if &receipt.file_name != expected_file {
        return false;
    }
    let subdir_is_consistent = match (
        identity.materials.get("artifact-subdir"),
        identity.material_options.get("catalog-subdir"),
    ) {
        (Some(material), Some(option)) => material == option,
        (None, None) => true,
        _ => false,
    };
    if !subdir_is_consistent {
        return false;
    }
    match (
        identity.materials.get("artifact-checksum"),
        identity.materials.get("artifact-url-blake3"),
    ) {
        (Some(expected), None) => {
            matching_locked_checksum(receipt.checksum.as_deref(), Some(expected))
        }
        (None, Some(expected)) => {
            github_artifact_url_hash(&receipt.url) == *expected
                && receipt
                    .checksum
                    .as_deref()
                    .is_none_or(|checksum| pipeline::parse_checksum(checksum).is_ok())
        }
        _ => false,
    }
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn matching_locked_checksum(actual: Option<&str>, expected: Option<&str>) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => {
            let Ok(actual) = pipeline::parse_checksum(actual) else {
                return false;
            };
            let Ok(expected) = pipeline::parse_checksum(expected) else {
                return false;
            };
            actual.algo == expected.algo && actual.hex.eq_ignore_ascii_case(&expected.hex)
        }
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
fn locked_artifact_url_must_match(actual: &str, expected: &str, checksum: Option<&str>) -> bool {
    checksum.is_some() || actual == expected
}

fn write_dynamic_inventory(locator: &InstallLocator) -> Result<()> {
    let install_root = locator.install_root().to_path_buf();
    let mut manifest = DynamicToolManifest::from_identity(locator.identity().clone())?;
    let bin = install_root.join("bin");
    let bin_dirs = if bin.exists() {
        vec![bin, install_root.clone()]
    } else {
        vec![install_root.clone()]
    };
    for bin_dir in bin_dirs {
        for name in crate::backend::bin_names_in_dirs(std::slice::from_ref(&bin_dir)) {
            let target = executable_in_dir(&bin_dir, &name).ok_or_else(|| {
                Error::other(format!("installed GitHub binary `{name}` disappeared"))
            })?;
            let canonical_root = dunce::canonicalize(&install_root)
                .map_err(|error| Error::io(&install_root, error))?;
            let canonical_target =
                dunce::canonicalize(&target).map_err(|error| Error::io(&target, error))?;
            let relative = canonical_target
                .strip_prefix(&canonical_root)
                .map_err(|_| {
                    Error::other(format!(
                        "installed GitHub binary `{name}` resolves outside {}",
                        install_root.display()
                    ))
                })?;
            manifest.bins.push(DynamicToolBin {
                name,
                path: relative.to_string_lossy().replace('\\', "/"),
            });
        }
    }
    manifest
        .bins
        .sort_by(|left, right| left.name.cmp(&right.name));
    manifest
        .bins
        .dedup_by(|left, right| left.name == right.name);
    manifest.write_atomic(&install_root)
}

fn finalize_dynamic_install(locator: &InstallLocator) -> Result<()> {
    let install_root = locator.install_root().to_path_buf();
    let marker = install_root.join(".osdk-complete");
    if let Err(error) = write_dynamic_inventory(locator) {
        let _ = std::fs::remove_dir_all(&install_root);
        return Err(error);
    }
    std::fs::write(&marker, b"").map_err(|error| {
        let _ = std::fs::remove_dir_all(&install_root);
        Error::io(&marker, error)
    })
}

fn executable_in_dir(directory: &std::path::Path, name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
        name.to_string(),
    ];
    #[cfg(not(windows))]
    let candidates = [name.to_string()];
    candidates
        .into_iter()
        .map(|candidate| directory.join(candidate))
        .find(|candidate| candidate.is_file())
}

fn tag_candidates(version: &str) -> Vec<String> {
    let mut tags = vec![version.to_string()];
    if !version.starts_with('v') {
        tags.push(format!("v{version}"));
    }
    tags
}

fn parse_release_tags(atom: &str, owner: &str, repo: &str) -> Vec<String> {
    if !looks_like_atom_feed(atom) {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut tags = Vec::new();
    for href in markup_attribute_values(atom, "href") {
        let href = decode_markup_entities(&href);
        let Ok(url) = reqwest::Url::parse(&href) else {
            continue;
        };
        if url.scheme() != "https" || url.host_str() != Some("github.com") {
            continue;
        }
        let Some(encoded_tag) = repository_release_tail(url.path(), owner, repo, "tag") else {
            continue;
        };
        if encoded_tag.is_empty() || encoded_tag.contains('/') {
            continue;
        }
        let Some(tag) = decode_url_segment(encoded_tag) else {
            continue;
        };
        if !tag.is_empty() && seen.insert(tag.clone()) {
            tags.push(tag);
        }
    }
    tags
}

fn parse_release_assets(html: &str, owner: &str, repo: &str, tag: &str) -> Vec<GhAsset> {
    if !looks_like_expanded_assets(html) {
        return Vec::new();
    }
    let base = reqwest::Url::parse("https://github.com/").expect("valid GitHub base URL");
    let mut seen = HashSet::new();
    let mut assets = Vec::new();
    for href in markup_attribute_values(html, "href") {
        let href = decode_markup_entities(&href);
        let Ok(url) = base.join(&href) else {
            continue;
        };
        if url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            continue;
        }
        let Some(tail) = repository_release_tail(url.path(), owner, repo, "download") else {
            continue;
        };
        let Some((encoded_tag, encoded_name)) = tail.split_once('/') else {
            continue;
        };
        if encoded_name.is_empty() || encoded_name.contains('/') {
            continue;
        }
        let Some(actual_tag) = decode_url_segment(encoded_tag) else {
            continue;
        };
        let Some(name) = decode_url_segment(encoded_name) else {
            continue;
        };
        if actual_tag != tag || name.is_empty() || name.contains(['/', '\\', '\0']) {
            continue;
        }
        let download_url = url.to_string();
        if seen.insert(download_url.clone()) {
            assets.push(GhAsset {
                name,
                browser_download_url: download_url,
            });
        }
    }
    assets
}

fn looks_like_atom_feed(markup: &str) -> bool {
    let lower = markup.to_ascii_lowercase();
    lower.contains("<feed")
        && lower.contains("http://www.w3.org/2005/atom")
        && lower.contains("<entry")
}

fn looks_like_expanded_assets(markup: &str) -> bool {
    let lower = markup.to_ascii_lowercase();
    lower.contains("<ul")
        && (lower.contains("list-style-none") || lower.contains("release-entry-list"))
}

fn repository_release_tail<'a>(
    path: &'a str,
    owner: &str,
    repo: &str,
    kind: &str,
) -> Option<&'a str> {
    let path = path.strip_prefix('/')?;
    let (actual_owner, path) = path.split_once('/')?;
    let (actual_repo, path) = path.split_once('/')?;
    let prefix = format!("releases/{kind}/");
    actual_owner
        .eq_ignore_ascii_case(owner)
        .then_some(())
        .and_then(|_| actual_repo.eq_ignore_ascii_case(repo).then_some(()))
        .and_then(|_| path.strip_prefix(&prefix))
}

fn markup_attribute_values(markup: &str, attribute: &str) -> Vec<String> {
    let bytes = markup.as_bytes();
    let needle = attribute.as_bytes();
    let mut values = Vec::new();
    let mut cursor = 0;
    while cursor + needle.len() < bytes.len() {
        let Some(offset) = bytes[cursor..]
            .windows(needle.len())
            .position(|window| window.eq_ignore_ascii_case(needle))
        else {
            break;
        };
        let start = cursor + offset;
        let boundary_before =
            start == 0 || bytes[start - 1].is_ascii_whitespace() || bytes[start - 1] == b'<';
        let mut index = start + needle.len();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if !boundary_before || bytes.get(index) != Some(&b'=') {
            cursor = start + needle.len();
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let Some(&quote @ (b'\'' | b'"')) = bytes.get(index) else {
            cursor = index;
            continue;
        };
        index += 1;
        let value_start = index;
        while index < bytes.len() && bytes[index] != quote {
            index += 1;
        }
        if index < bytes.len() {
            values.push(String::from_utf8_lossy(&bytes[value_start..index]).into_owned());
            cursor = index + 1;
        } else {
            break;
        }
    }
    values
}

fn decode_markup_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn decode_url_segment(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push(hex_value(high)? * 16 + hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    (!decoded.chars().any(char::is_control)).then_some(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn select_asset(
    backend: &GithubBackend,
    assets: &[GhAsset],
    version: &str,
    ctx: &Ctx,
    rules: &AssetRules,
) -> Result<GhAsset> {
    if let Some(template) = &rules.template {
        let rendered = render_asset_template_version(template, version, ctx, rules);
        let matching: Vec<_> = assets
            .iter()
            .filter(|asset| asset.name == rendered)
            .cloned()
            .collect();
        return unique_asset(matching, "asset-template");
    }
    if let Some(regex) = &rules.regex {
        let matching = assets
            .iter()
            .filter(|asset| regex.is_match(&asset.name))
            .cloned()
            .collect();
        return unique_asset(matching, "asset-regex");
    }
    let mut scored = assets
        .iter()
        .filter_map(|asset| {
            backend
                .score_asset(&asset.name, ctx, Some(rules))
                .map(|score| (score, asset.clone()))
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|item| item.0);
    scored.pop().map(|(_, asset)| asset).ok_or_else(|| {
        Error::other(format!(
            "no release asset for {} matches this platform ({})",
            backend.id(),
            ctx.platform
        ))
    })
}

fn unique_asset(assets: Vec<GhAsset>, rule: &str) -> Result<GhAsset> {
    if assets.len() != 1 {
        return Err(Error::other(format!(
            "{rule} matched {} assets (expected exactly 1)",
            assets.len()
        )));
    }
    Ok(assets.into_iter().next().unwrap())
}

fn render_asset_template(template: &str, ctx: &Ctx, rules: &AssetRules) -> String {
    template
        .replace("{os}", rules.os.as_deref().unwrap_or_else(|| os_token(ctx)))
        .replace(
            "{arch}",
            rules.arch.as_deref().unwrap_or_else(|| arch_token(ctx)),
        )
        .replace(
            "{libc}",
            rules.libc.as_deref().unwrap_or_else(|| libc_token(ctx)),
        )
}

fn render_asset_template_version(
    template: &str,
    version: &str,
    ctx: &Ctx,
    rules: &AssetRules,
) -> String {
    render_asset_template(template, ctx, rules).replace("{version}", version)
}

fn static_asset_matches(asset: &StaticAsset, ctx: &Ctx, rules: &AssetRules) -> bool {
    asset.os == rules.os.as_deref().unwrap_or_else(|| os_token(ctx))
        && asset.arch == rules.arch.as_deref().unwrap_or_else(|| arch_token(ctx))
        && asset
            .libc
            .as_deref()
            .is_none_or(|libc| libc == rules.libc.as_deref().unwrap_or_else(|| libc_token(ctx)))
}

fn static_asset_url_is_persistable(value: &str) -> bool {
    reqwest::Url::parse(value).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && !url.cannot_be_a_base()
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn static_versions(catalog: &StaticCatalog) -> Vec<VersionInfo> {
    let mut versions = catalog
        .releases
        .iter()
        .map(|release| VersionInfo {
            version: release.tag.trim_start_matches('v').into(),
            stable: !release.prerelease,
            lts: None,
        })
        .collect::<Vec<_>>();
    versions
        .sort_by(|left, right| crate::backend::python::cmp_versions(&left.version, &right.version));
    versions
}

fn os_token(ctx: &Ctx) -> &'static str {
    match ctx.platform.os {
        Os::Linux => "linux",
        Os::Macos => "macos",
        Os::Windows => "windows",
    }
}

fn arch_token(ctx: &Ctx) -> &'static str {
    match ctx.platform.arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "arm64",
        Arch::X86 => "x86",
        Arch::Arm => "arm",
    }
}

fn libc_token(ctx: &Ctx) -> &'static str {
    match ctx.platform.libc {
        crate::platform::Libc::Glibc => "gnu",
        crate::platform::Libc::Musl => "musl",
        crate::platform::Libc::None => "none",
    }
}

fn verify_catalog_bytes(source: &str, expected: &str, bytes: &[u8]) -> Result<()> {
    let actual = pipeline::verify::hash_bytes(bytes, pipeline::HashAlgo::Sha256);
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(Error::ChecksumMismatch {
            name: source.into(),
            expected: expected.into(),
            actual,
        })
    }
}

fn static_catalog_cache(ctx: &Ctx, digest: &str) -> PathBuf {
    ctx.dirs
        .remote_cache()
        .join(format!("github-static-catalog-{}.json", digest.trim()))
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes).map_err(|error| Error::io(&temporary, error))?;
    std::fs::rename(&temporary, path).map_err(|error| Error::io(path, error))
}

fn postprocess_archive(ctx: &Ctx, locator: &InstallLocator, rules: &AssetRules) -> Result<()> {
    if rules.bins.is_empty() && rules.rename.is_none() && rules.strip_components == 0 {
        return Ok(());
    }
    let install = locator.install_root().to_path_buf();
    let result = (|| {
        let base = strip_components_root(&install, rules.strip_components)?;
        let bin_dir = install.join("bin");
        crate::dirs::create_dir_all(&bin_dir)?;
        let bins = if rules.bins.is_empty() {
            Vec::new()
        } else {
            rules.bins.clone()
        };
        if rules.rename.is_some() && bins.len() != 1 {
            return Err(Error::config(
                "rename requires exactly one bin or bins entry",
            ));
        }
        for source in bins {
            let source_path = base.join(&source);
            if !source_path.is_file() {
                return Err(Error::other(format!(
                    "configured GitHub binary is missing: {}",
                    source.display()
                )));
            }
            let name = rules
                .rename
                .as_deref()
                .map(str::to_string)
                .or_else(|| {
                    source
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .ok_or_else(|| Error::config("configured bin has no filename"))?;
            let destination = bin_dir.join(normalize_executable_name(&name, ctx.platform.os));
            std::fs::copy(&source_path, &destination)
                .map_err(|error| Error::io(&destination, error))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))
                    .map_err(|error| Error::io(&destination, error))?;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&install);
        return Err(error);
    }
    Ok(())
}

fn strip_components_root(root: &std::path::Path, count: usize) -> Result<PathBuf> {
    let mut selected = root.to_path_buf();
    for _ in 0..count {
        let mut children = std::fs::read_dir(&selected)
            .map_err(|error| Error::io(&selected, error))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with(".osdk-"))
            .collect::<Vec<_>>();
        if children.len() != 1 || !children[0].path().is_dir() {
            return Err(Error::other(format!(
                "strip-components cannot descend through {}",
                selected.display()
            )));
        }
        selected = children.remove(0).path();
    }
    Ok(selected)
}

fn normalize_executable_name(name: &str, os: Os) -> String {
    if os == Os::Windows && !name.to_ascii_lowercase().ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn mentions_other_os_token(name: &str, os: &str) -> bool {
    let others: &[&str] = match os {
        "linux" => &["darwin", "apple", "macos", "windows", ".exe"],
        "macos" | "darwin" => &["linux", "windows", ".exe"],
        "windows" | "win" => &["linux", "darwin", "apple", "macos"],
        _ => &[],
    };
    others.iter().any(|o| name.contains(o))
}

fn mentions_other_arch_token(name: &str, arch: &str) -> bool {
    let others: &[&str] = match arch {
        "x64" | "x86_64" | "amd64" => &["aarch64", "arm64"],
        "arm64" | "aarch64" => &["x86_64", "amd64"],
        "x86" | "i686" => &["aarch64", "arm64", "x86_64", "amd64"],
        "arm" | "armv7" => &["aarch64", "x86_64", "amd64"],
        _ => &[],
    };
    others.iter().any(|o| name.contains(o))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, Settings, SourcesConfig, ToolSources};
    use crate::dirs::Dirs;
    use crate::platform::{Libc, Platform};
    use crate::source::{Selection, Source};
    use crate::store::Cas;

    #[test]
    fn parse_id() {
        let b = GithubBackend::from_id("github:cli/cli").unwrap();
        assert_eq!(b.owner, "cli");
        assert_eq!(b.repo, "cli");
        assert_eq!(b.id(), "github:cli/cli");
        assert_eq!(
            GithubBackend::from_id("github:Cli/CLI.git").unwrap().id(),
            "github:cli/cli"
        );
        assert!(GithubBackend::from_id("github:noslash").is_none());
        assert!(GithubBackend::from_id("node").is_none());
        assert!(GithubBackend::from_id("github:cli/cli/extra").is_none());
        assert!(GithubBackend::from_id("github:../cli").is_none());
        assert!(GithubBackend::from_id("github:cli/repo?ref=bad").is_none());
    }

    #[test]
    fn default_sources_cover_direct_and_full_ghproxy_routes() {
        let backend = GithubBackend::from_id("github:cli/cli").unwrap();
        let sources = backend.default_sources();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].download_url, "https://github.com/");
        assert_eq!(
            sources[1].download_url,
            "https://gh-proxy.com/https://github.com/"
        );
        assert_eq!(
            sources[1].index_url.as_deref(),
            Some("https://gh-proxy.com/https://api.github.com/")
        );
    }

    fn test_ctx(root: &std::path::Path) -> Ctx {
        let dirs = Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            "OSDK_STORE_DIR" => Some(root.join("store").display().to_string()),
            "OSDK_INSTALL_DIR" => Some(root.join("installs").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            cas: Arc::new(Cas::new(dirs.store.clone())),
            dirs,
            platform: Platform {
                os: Os::Linux,
                arch: Arch::X64,
                libc: Libc::Glibc,
            },
            config: Config {
                settings: Settings::default(),
                sources: SourcesConfig {
                    selection: Selection::Ordered,
                    ..Default::default()
                },
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    #[test]
    fn explicit_asset_rules_require_exactly_one_match_and_render_targets() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let assets = vec![
            GhAsset {
                name: "tool-1.2.3-linux-x64.tar.gz".into(),
                browser_download_url: "https://example.test/x64".into(),
            },
            GhAsset {
                name: "tool-1.2.3-linux-arm64.tar.gz".into(),
                browser_download_url: "https://example.test/arm64".into(),
            },
        ];
        let template = AssetRules {
            template: Some("tool-{version}-{os}-{arch}.tar.gz".into()),
            regex: None,
            bins: Vec::new(),
            rename: None,
            strip_components: 0,
            os: Some("linux".into()),
            arch: Some("arm64".into()),
            libc: None,
        };
        assert_eq!(
            select_asset(&backend, &assets, "1.2.3", &ctx, &template)
                .unwrap()
                .name,
            "tool-1.2.3-linux-arm64.tar.gz"
        );
        for expression in ["nomatch", "tool-.*"] {
            let regex = AssetRules {
                regex: Some(regex::Regex::new(expression).unwrap()),
                template: None,
                bins: Vec::new(),
                rename: None,
                strip_components: 0,
                os: None,
                arch: None,
                libc: None,
            };
            assert!(select_asset(&backend, &assets, "1.2.3", &ctx, &regex).is_err());
        }
    }

    #[test]
    fn executable_rename_must_be_a_single_safe_filename() {
        for rename in [
            "../../outside",
            "/outside",
            r"..\outside",
            r"C:\outside.exe",
            ".",
            "..",
        ] {
            let options = std::collections::BTreeMap::from([("rename".into(), rename.into())]);
            assert!(GithubBackend::rules(&options).is_err(), "{rename}");
        }

        for rename in ["tool", "tool.exe"] {
            let options = std::collections::BTreeMap::from([("rename".into(), rename.into())]);
            assert_eq!(
                GithubBackend::rules(&options).unwrap().rename.as_deref(),
                Some(rename)
            );
        }
    }

    #[test]
    fn public_metadata_parsers_keep_only_repository_scoped_release_links() {
        let atom = r#"
            <feed xmlns='http://www.w3.org/2005/Atom'>
              <entry><link type='text/html' rel='alternate'
                href='https://github.com/example/tool/releases/tag/v2.0.0-beta.1'/></entry>
              <entry><link href="https://github.com/example/tool/releases/tag/release%2F2026-08"/></entry>
              <entry><link href='https://evil.example/example/tool/releases/tag/v9'/></entry>
              <entry><link href='https://github.com/example/other/releases/tag/v8'/></entry>
              <entry><link href='https://github.com/EXAMPLE/TOOL/releases/tag/v2.0.0-beta.1'/></entry>
            </feed>
        "#;
        assert_eq!(
            parse_release_tags(atom, "example", "tool"),
            vec!["v2.0.0-beta.1", "release/2026-08"]
        );

        let html = r#"
            <ul class='list-style-none'>
            <a data-turbo='false' href='/example/tool/releases/download/v1.2.3/tool-linux-x86_64.tar.gz'>tool</a>
            <a href="https://github.com/EXAMPLE/TOOL/releases/download/v1.2.3/tool%20symbols.zip?download=1&amp;x=2">symbols</a>
            <a href='/example/tool/archive/refs/tags/v1.2.3.zip'>source</a>
            <a href='//evil.example/example/tool/releases/download/v1.2.3/evil'>evil</a>
            <a href='/example/tool-malicious/releases/download/v1.2.3/evil'>lookalike</a>
            <a href='/example/tool/releases/download/v9/wrong-tag'>wrong tag</a>
            </ul>
        "#;
        let assets = parse_release_assets(html, "example", "tool", "v1.2.3");
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0].name, "tool-linux-x86_64.tar.gz");
        assert_eq!(assets[1].name, "tool symbols.zip");
        assert!(!assets[1].browser_download_url.contains("&amp;"));

        assert!(parse_release_tags(
            "<html><a href='https://github.com/example/tool/releases/tag/v9'>retry</a></html>",
            "example",
            "tool",
        )
        .is_empty());
        assert!(parse_release_assets(
            "<html><a href='/example/tool/releases/download/v9/tool.tar.gz'>retry</a></html>",
            "example",
            "tool",
            "v9",
        )
        .is_empty());
    }

    #[tokio::test]
    async fn offline_exact_release_reuses_paginated_release_cache() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let api = backend.releases_api(1);
        let cache = http::metadata_cache_path(&ctx, &api);
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(
            cache,
            br#"[{"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":[{"name":"tool-linux-x86_64.tar.gz","browser_download_url":"https://github.com/example/tool/releases/download/v1.2.3/tool-linux-x86_64.tar.gz"}]}]"#,
        )
        .unwrap();
        ctx.config.settings.offline = true;
        let release = backend
            .release_for_tag(&ctx, &backend.default_sources(), "1.2.3")
            .await
            .unwrap();
        assert_eq!(release.tag_name, "v1.2.3");
        assert_eq!(release.assets.len(), 1);
    }

    #[tokio::test]
    async fn anonymous_api_rate_limit_falls_back_to_public_atom() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for response in [
                (
                    "403 Forbidden",
                    "application/json",
                    "X-RateLimit-Limit: 60\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1787446800\r\n",
                    r#"{"message":"API rate limit exceeded"}"#,
                ),
                (
                    "200 OK",
                    "application/atom+xml",
                    "",
                    r#"<feed xmlns='http://www.w3.org/2005/Atom'><entry><link href='https://github.com/example/tool/releases/tag/v2.0.0'/></entry><entry><link href='https://github.com/example/tool/releases/tag/v2.1.0-beta.1'/></entry></feed>"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                while !request.ends_with(b"\r\n\r\n") {
                    let size = stream.read(&mut buffer).unwrap();
                    if size == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..size]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
                write!(
                    stream,
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.0,
                    response.1,
                    response.2,
                    response.3.len(),
                    response.3,
                )
                .unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.sources.per_tool.insert(
            "github:example/tool".into(),
            ToolSources {
                pin: Some("fixture".into()),
                disable: vec!["github".into(), "ghproxy".into()],
                custom: vec![Source::official("fixture", &format!("http://{address}/"))
                    .with_index(&format!("http://{address}/"))],
                ..Default::default()
            },
        );
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let versions = backend.list_remote_versions(&ctx).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, "2.1.0-beta.1");
        assert!(!versions[0].stable);
        assert_eq!(versions[1].version, "2.0.0");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exact_release_asset_discovery_falls_back_to_public_html() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for (status, headers, body) in [
                (
                    "403 Forbidden",
                    "Content-Type: application/json\r\nX-GitHub-Request-Id: fixture-secondary\r\nX-RateLimit-Remaining: 0\r\nRetry-After: 30\r\n",
                    r#"{"message":"You have exceeded a secondary rate limit."}"#,
                ),
                (
                    "200 OK",
                    "Content-Type: text/html\r\n",
                    r#"<ul class='list-style-none'><li><a href='/example/tool/releases/download/1.2.3/tool-linux-x86_64.tar.gz'>tool</a></li><li><a href='/example/tool/archive/refs/tags/1.2.3.zip'>source</a></li></ul>"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                while !request.ends_with(b"\r\n\r\n") {
                    let size = stream.read(&mut buffer).unwrap();
                    if size == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..size]);
                }
                assert!(
                    !String::from_utf8(request)
                        .unwrap()
                        .to_ascii_lowercase()
                        .contains("authorization:")
                );
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let sources = vec![Source::official("fixture", &format!("http://{address}/"))
            .with_index(&format!("http://{address}/"))];
        let release = backend
            .release_for_tag(&ctx, &sources, "1.2.3")
            .await
            .unwrap();
        assert_eq!(release.tag_name, "1.2.3");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].name, "tool-linux-x86_64.tar.gz");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn release_pagination_finds_versions_on_second_page() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for page in 1..=2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                while !request.ends_with(b"\r\n\r\n") {
                    let size = stream.read(&mut buffer).unwrap();
                    if size == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..size]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.contains(&format!("page={page}")));
                let body = if page == 1 {
                    serde_json::to_string(
                        &(0..100)
                            .map(|index| {
                                serde_json::json!({
                                    "tag_name": format!("v1.0.{index}"),
                                    "draft": false,
                                    "prerelease": false,
                                    "assets": []
                                })
                            })
                            .collect::<Vec<_>>(),
                    )
                    .unwrap()
                } else {
                    r#"[{"tag_name":"v2.0.0","draft":false,"prerelease":false,"assets":[]}]"#.into()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.sources.per_tool.insert(
            "github:example/tool".into(),
            ToolSources {
                pin: Some("fixture".into()),
                disable: vec!["github".into(), "ghproxy".into()],
                custom: vec![Source::official("fixture", &format!("http://{address}/"))
                    .with_index(&format!("http://{address}/"))],
                ..Default::default()
            },
        );
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let versions = backend.list_remote_versions(&ctx).await.unwrap();
        assert!(versions.iter().any(|version| version.version == "2.0.0"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn static_catalog_resolves_without_releases_api_and_locks_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let catalog_path = temp.path().join("catalog.json");
        let catalog = StaticCatalog {
            schema: 1,
            releases: vec![StaticRelease {
                tag: "v1.2.3".into(),
                prerelease: false,
                assets: vec![StaticAsset {
                    name: "tool-linux-x64.tar.gz".into(),
                    url: "https://artifacts.example/tool.tar.gz".into(),
                    checksum: format!("sha256:{}", "a".repeat(64)),
                    os: "linux".into(),
                    arch: "x64".into(),
                    libc: Some("gnu".into()),
                }],
            }],
        };
        let bytes = serde_json::to_vec_pretty(&catalog).unwrap();
        std::fs::write(&catalog_path, &bytes).unwrap();
        let digest = pipeline::verify::hash_bytes(&bytes, pipeline::HashAlgo::Sha256);
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let mut request = ToolRequest::parse("github:example/tool@latest").unwrap();
        request
            .options
            .insert("catalog-url".into(), catalog_path.display().to_string());
        request.options.insert("catalog-sha256".into(), digest);
        let resolved = backend.resolve_version(&ctx, &request).await.unwrap();
        assert_eq!(resolved.version, "1.2.3");
        assert_eq!(
            resolved.options[pipeline::LOCKED_ARTIFACT_URL_OPTION],
            "https://artifacts.example/tool.tar.gz"
        );
    }

    #[test]
    fn static_catalog_asset_urls_must_be_safe_to_persist() {
        assert!(static_asset_url_is_persistable(
            "https://artifacts.example/tool.tar.gz"
        ));
        for unsafe_url in [
            "https://user:secret@artifacts.example/tool.tar.gz",
            "https://artifacts.example/tool.tar.gz?token=secret",
            "https://artifacts.example/tool.tar.gz#fragment",
        ] {
            assert!(
                !static_asset_url_is_persistable(unsafe_url),
                "accepted {unsafe_url}"
            );
        }
    }

    #[test]
    fn checksumless_signed_url_is_hashed_in_install_manifest_identity() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let secret_url = "https://artifacts.example/tool?token=do-not-persist";
        let version = locked_version(&backend, "1.2.3", secret_url, "tool", None);
        let locator = github_install_locator(&ctx, backend.id(), &version).unwrap();
        let manifest = DynamicToolManifest::from_identity(locator.identity().clone()).unwrap();
        let json = serde_json::to_string(&manifest).unwrap();

        assert!(!json.contains(secret_url));
        assert!(!json.contains("do-not-persist"));
        assert!(!manifest.identity.materials.contains_key("artifact-url"));
        assert_eq!(
            manifest.identity.materials["artifact-url-blake3"],
            github_artifact_url_hash(secret_url)
        );
    }

    #[tokio::test]
    async fn static_catalog_locked_artifact_installs_offline_with_multiple_bins() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("tool.tar.gz");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            for (path, contents) in [
                ("release/pkg/a", b"a".as_slice()),
                ("release/pkg/b", b"b".as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, contents).unwrap();
            }
            builder.finish().unwrap();
        }
        let checksum = pipeline::verify::hash_file(&archive, pipeline::HashAlgo::Sha256).unwrap();
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.settings.offline = true;
        let file_name = "tool.tar.gz";
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.extend(std::collections::BTreeMap::from([
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://invalid.example/tool.tar.gz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                file_name.into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{checksum}"),
            ),
            ("bins".into(), "pkg/a,pkg/b".into()),
            ("strip-components".into(), "1".into()),
        ]));
        let locator = github_install_locator(&ctx, backend.id(), &version).unwrap();
        let cached = pipeline::dynamic_artifact_cache_path(&ctx.dirs, &locator, file_name).unwrap();
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::copy(&archive, &cached).unwrap();
        backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap();
        let install = locator.install_root();
        assert!(install.join("bin/a").is_file());
        assert!(install.join("bin/b").is_file());
        assert!(install.join(".osdk-complete").is_file());
        let manifest = DynamicToolManifest::load(install).unwrap();
        assert_eq!(manifest.schema, 1);
        assert!(manifest.matches_identity(locator.identity()));
        let mut changed = version.clone();
        changed.options.insert("bins".into(), "pkg/a".into());
        let changed_locator = github_install_locator(&ctx, backend.id(), &changed).unwrap();
        assert_ne!(locator.install_root(), changed_locator.install_root());
        assert!(!manifest.matches_identity(changed_locator.identity()));
    }

    fn complete_install_fixture(
        ctx: &Ctx,
        backend: &GithubBackend,
        version: &ToolVersion,
        contents: &[u8],
    ) -> InstallLocator {
        let locator = github_install_locator(ctx, backend.id(), version).unwrap();
        let install = locator.install_root();
        std::fs::create_dir_all(install.join("bin")).unwrap();
        std::fs::write(install.join("bin/tool"), contents).unwrap();
        std::fs::write(install.join(".osdk-complete"), b"").unwrap();
        locator
    }

    fn locked_version(
        backend: &GithubBackend,
        version: &str,
        url: &str,
        file_name: &str,
        checksum: Option<&str>,
    ) -> ToolVersion {
        let mut version = ToolVersion::new(backend.id(), version);
        version
            .options
            .insert(pipeline::LOCKED_ARTIFACT_URL_OPTION.into(), url.into());
        version.options.insert(
            pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
            file_name.into(),
        );
        if let Some(checksum) = checksum {
            version.options.insert(
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                checksum.into(),
            );
        }
        version
    }

    fn write_complete_install_fixture(
        ctx: &Ctx,
        backend: &GithubBackend,
        version: &ToolVersion,
        contents: &[u8],
    ) -> InstallLocator {
        let locator = complete_install_fixture(ctx, backend, version, contents);
        let install = locator.install_root();
        let mut manifest = DynamicToolManifest::from_identity(locator.identity().clone()).unwrap();
        manifest.bins.push(DynamicToolBin {
            name: "tool".into(),
            path: "bin/tool".into(),
        });
        manifest.write_atomic(install).unwrap();
        let receipt = pipeline::ArtifactReceipt {
            url: version.options[pipeline::LOCKED_ARTIFACT_URL_OPTION].clone(),
            file_name: version.options[pipeline::LOCKED_ARTIFACT_FILE_OPTION].clone(),
            checksum: version
                .options
                .get(pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION)
                .cloned(),
            evidence: Vec::new(),
        };
        std::fs::write(
            install.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        locator
    }

    #[test]
    fn locked_locator_restarts_with_the_same_material_identity() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let checksum = format!("sha256:{}", "a".repeat(64));
        let version = locked_version(
            &backend,
            "1.2.3",
            "https://example.test/tool",
            "tool",
            Some(&checksum),
        );
        let first = github_install_locator(&ctx, backend.id(), &version).unwrap();
        let restarted = github_install_locator(&ctx, backend.id(), &version).unwrap();
        assert_eq!(first.identity(), restarted.identity());
        assert_eq!(first.install_root(), restarted.install_root());
    }

    #[test]
    fn unlocked_locator_recovers_one_valid_material_identity_and_rejects_ambiguity() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let first = locked_version(
            &backend,
            "1.2.3",
            "https://example.test/tool-a",
            "tool",
            None,
        );
        let first_locator =
            write_complete_install_fixture(&ctx, &backend, &first, b"first material");
        let unlocked = ToolVersion::new(backend.id(), "1.2.3");
        assert_eq!(
            github_install_locator(&ctx, backend.id(), &unlocked)
                .unwrap()
                .install_root(),
            first_locator.install_root()
        );

        let second = locked_version(
            &backend,
            "1.2.3",
            "https://example.test/tool-b",
            "tool",
            None,
        );
        let second_locator =
            write_complete_install_fixture(&ctx, &backend, &second, b"second material");
        assert_ne!(first_locator.install_root(), second_locator.install_root());
        let error = github_install_locator(&ctx, backend.id(), &unlocked).unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
        assert!(error.to_string().contains("lockfile"));
    }

    #[tokio::test]
    async fn uninstall_removes_only_the_exact_material_variant() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let first = locked_version(
            &backend,
            "1.2.3",
            "https://example.test/tool-a",
            "tool",
            None,
        );
        let second = locked_version(
            &backend,
            "1.2.3",
            "https://example.test/tool-b",
            "tool",
            None,
        );
        let first_locator = write_complete_install_fixture(&ctx, &backend, &first, b"first");
        let second_locator = write_complete_install_fixture(&ctx, &backend, &second, b"second");
        let version_root = first_locator.install_root().parent().unwrap().to_path_buf();

        backend.uninstall(&ctx, &first).await.unwrap();

        assert!(!first_locator.install_root().exists());
        assert!(second_locator.install_root().join("bin/tool").is_file());
        assert!(version_root.is_dir());
    }

    #[test]
    fn list_installed_uses_only_valid_current_manifests_and_dedupes_versions() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        for url in ["https://example.test/tool-a", "https://example.test/tool-b"] {
            let version = locked_version(&backend, "1.2.3", url, "tool", None);
            write_complete_install_fixture(&ctx, &backend, &version, url.as_bytes());
        }
        let current = locked_version(
            &backend,
            "2.0.0",
            "https://example.test/tool-2",
            "tool",
            None,
        );
        write_complete_install_fixture(&ctx, &backend, &current, b"two");

        let invalid = locked_version(
            &backend,
            "3.0.0",
            "https://example.test/tool-3",
            "tool",
            None,
        );
        let invalid_locator = write_complete_install_fixture(&ctx, &backend, &invalid, b"invalid");
        std::fs::remove_file(invalid_locator.install_root().join(".osdk-artifact.json")).unwrap();

        let legacy = ctx.dirs.install_path(backend.id(), "4.0.0");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join(".osdk-complete"), b"").unwrap();
        std::fs::write(legacy.join(".osdk-tool.json"), b"{}").unwrap();

        assert_eq!(
            backend.list_installed(&ctx).unwrap(),
            vec!["1.2.3", "2.0.0"]
        );
    }

    #[tokio::test]
    async fn install_refuses_complete_install_without_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let version = locked_version(&backend, "1.2.3", "https://example.test/tool", "tool", None);
        let locator = complete_install_fixture(&ctx, &backend, &version, b"original bytes");
        let install = locator.install_root();

        let error = backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("refusing to reuse complete"));
        assert!(error.to_string().contains("missing, legacy, or invalid"));
        assert_eq!(
            std::fs::read(install.join("bin/tool")).unwrap(),
            b"original bytes"
        );
        assert!(!DynamicToolManifest::manifest_path(install).exists());
    }

    #[tokio::test]
    async fn legacy_inventory_is_never_reused() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.settings.offline = true;
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let file_name = "tool";
        let checksum = pipeline::verify::hash_bytes(b"fresh bytes", pipeline::HashAlgo::Sha256);
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.extend(std::collections::BTreeMap::from([
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://invalid.example/tool".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                file_name.into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{checksum}"),
            ),
        ]));
        let locator = github_install_locator(&ctx, backend.id(), &version).unwrap();
        let cached = pipeline::dynamic_artifact_cache_path(&ctx.dirs, &locator, file_name).unwrap();
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"fresh bytes").unwrap();
        let legacy_install = locator.legacy_install_root();
        std::fs::create_dir_all(legacy_install.join("bin")).unwrap();
        std::fs::write(legacy_install.join("bin/tool"), b"legacy bytes").unwrap();
        std::fs::write(legacy_install.join(".osdk-complete"), b"").unwrap();
        let legacy_inventory = legacy_install.join(".osdk-tool.json");
        let legacy_contents = r#"{"schema":1,"id":"github:example/tool","version":"1.2.3","bins":[{"name":"tool","path":"bin/tool"}]}"#;
        std::fs::write(&legacy_inventory, legacy_contents).unwrap();
        assert!(!locator.install_root().exists());

        backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(legacy_install.join("bin/tool")).unwrap(),
            b"legacy bytes"
        );
        assert_eq!(
            std::fs::read(&legacy_inventory).unwrap(),
            legacy_contents.as_bytes()
        );
        assert!(!DynamicToolManifest::manifest_path(legacy_install).exists());
        assert_ne!(locator.install_root(), legacy_install);
        assert!(locator.install_root().join(".osdk-complete").is_file());
        assert_eq!(
            std::fs::read(locator.install_root().join("bin/tool")).unwrap(),
            b"fresh bytes"
        );
        let manifest = DynamicToolManifest::load(locator.install_root()).unwrap();
        assert!(manifest.matches_identity(locator.identity()));
    }

    #[tokio::test]
    async fn install_refuses_complete_install_with_mismatched_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let mut installed =
            locked_version(&backend, "1.2.3", "https://example.test/tool", "tool", None);
        installed.options.insert("rename".into(), "old-name".into());
        let mut requested = installed.clone();
        requested.options.insert("rename".into(), "new-name".into());
        let locator = complete_install_fixture(&ctx, &backend, &requested, b"original bytes");
        let install = locator.install_root();
        let installed_locator = github_install_locator(&ctx, backend.id(), &installed).unwrap();
        assert_ne!(installed_locator.install_root(), install);
        let mut manifest =
            DynamicToolManifest::from_identity(installed_locator.identity().clone()).unwrap();
        manifest.bins.push(DynamicToolBin {
            name: "tool".into(),
            path: "bin/tool".into(),
        });
        manifest.write_atomic(install).unwrap();
        let original_inventory =
            std::fs::read(DynamicToolManifest::manifest_path(install)).unwrap();

        let error = backend
            .install(&InstallCtx { ctx: &ctx }, &requested)
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("missing, legacy, or invalid install identity"));
        assert_eq!(
            std::fs::read(install.join("bin/tool")).unwrap(),
            b"original bytes"
        );
        assert_eq!(
            std::fs::read(DynamicToolManifest::manifest_path(install)).unwrap(),
            original_inventory
        );
        let persisted = DynamicToolManifest::load(install).unwrap();
        assert!(persisted.matches_identity(installed_locator.identity()));
        assert!(!persisted.matches_identity(locator.identity()));
    }

    #[tokio::test]
    async fn install_refuses_complete_install_with_different_locked_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let mut version = ToolVersion::new(backend.id(), "1.2.3");
        version.options.extend(std::collections::BTreeMap::from([
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://invalid.example/new-tool.tar.gz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "new-tool.tar.gz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{}", "b".repeat(64)),
            ),
        ]));
        let locator = complete_install_fixture(&ctx, &backend, &version, b"original bytes");
        let install = locator.install_root();
        let mut manifest = DynamicToolManifest::from_identity(locator.identity().clone()).unwrap();
        manifest.bins.push(DynamicToolBin {
            name: "tool".into(),
            path: "bin/tool".into(),
        });
        manifest.write_atomic(install).unwrap();
        std::fs::write(
            install.join(".osdk-artifact.json"),
            serde_json::to_vec_pretty(&pipeline::ArtifactReceipt {
                url: "https://invalid.example/old-tool.tar.gz".into(),
                file_name: "old-tool.tar.gz".into(),
                checksum: Some(format!("sha256:{}", "a".repeat(64))),
                evidence: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let error = backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("invalid receipt"));
        assert_eq!(
            std::fs::read(install.join("bin/tool")).unwrap(),
            b"original bytes"
        );
    }

    #[test]
    fn checksumless_locked_artifact_requires_the_same_url() {
        assert!(locked_artifact_url_must_match(
            "https://example.test/tool",
            "https://example.test/tool",
            None,
        ));
        assert!(!locked_artifact_url_must_match(
            "https://mirror.example.test/tool",
            "https://example.test/tool",
            None,
        ));
        assert!(locked_artifact_url_must_match(
            "https://mirror.example.test/tool",
            "https://example.test/tool",
            Some("sha256:00"),
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_same_version_distinct_option_installs_coexist() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path());
        ctx.config.settings.offline = true;
        let ctx = Arc::new(ctx);
        let backend = Arc::new(GithubBackend::from_id("github:example/tool").unwrap());
        let archive = temp.path().join("tool.tar.gz");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            for (path, contents) in [
                ("release/pkg/a", b"first".as_slice()),
                ("release/pkg/b", b"second".as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, path, contents).unwrap();
            }
            builder.finish().unwrap();
        }
        let checksum = pipeline::verify::hash_file(&archive, pipeline::HashAlgo::Sha256).unwrap();
        let file_name = "tool.tar.gz";
        let make_version = |bin: &str| {
            let mut version = ToolVersion::new(backend.id(), "1.2.3");
            version.options.extend(std::collections::BTreeMap::from([
                (
                    pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                    "https://invalid.example/tool.tar.gz".into(),
                ),
                (
                    pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                    file_name.into(),
                ),
                (
                    pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                    format!("sha256:{checksum}"),
                ),
                ("bins".into(), bin.into()),
                ("strip-components".into(), "1".into()),
            ]));
            version
        };
        let first = make_version("pkg/a");
        let second = make_version("pkg/b");
        for version in [&first, &second] {
            let locator = github_install_locator(&ctx, backend.id(), version).unwrap();
            let cached =
                pipeline::dynamic_artifact_cache_path(&ctx.dirs, &locator, file_name).unwrap();
            std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
            std::fs::copy(&archive, cached).unwrap();
        }
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let first_task = {
            let ctx = Arc::clone(&ctx);
            let backend = Arc::clone(&backend);
            let barrier = Arc::clone(&barrier);
            let version = first.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                backend.install(&InstallCtx { ctx: &ctx }, &version).await
            })
        };
        let second_task = {
            let ctx = Arc::clone(&ctx);
            let backend = Arc::clone(&backend);
            let barrier = Arc::clone(&barrier);
            let version = second.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                backend.install(&InstallCtx { ctx: &ctx }, &version).await
            })
        };
        let first_result = first_task.await.unwrap();
        let second_result = second_task.await.unwrap();
        first_result.unwrap();
        second_result.unwrap();

        let first_locator = github_install_locator(&ctx, backend.id(), &first).unwrap();
        let second_locator = github_install_locator(&ctx, backend.id(), &second).unwrap();
        assert_ne!(first_locator.install_root(), second_locator.install_root());

        let first_install = first_locator.install_root();
        let first_manifest = DynamicToolManifest::load(first_install).unwrap();
        assert!(first_manifest.matches_identity(first_locator.identity()));
        assert!(first_install.join("bin/a").is_file());
        assert!(!first_install.join("bin/b").is_file());

        let second_install = second_locator.install_root();
        let second_manifest = DynamicToolManifest::load(second_install).unwrap();
        assert!(second_manifest.matches_identity(second_locator.identity()));
        assert!(second_install.join("bin/b").is_file());
        assert!(!second_install.join("bin/a").is_file());
    }

    #[test]
    fn postprocess_is_atomic_for_multiple_binaries_and_windows_names() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let version = ToolVersion::new(backend.id(), "1.0.0");
        let asset = GhAsset {
            name: "tool.tar.gz".into(),
            browser_download_url: "https://example.test/tool-1".into(),
        };
        let locator =
            github_install_locator_for_artifact(&ctx, backend.id(), &version, &asset, None)
                .unwrap();
        let install = locator.install_root();
        std::fs::create_dir_all(install.join("release/pkg")).unwrap();
        std::fs::write(install.join("release/pkg/a"), b"a").unwrap();
        std::fs::write(install.join("release/pkg/b"), b"b").unwrap();
        let rules = AssetRules {
            regex: None,
            template: None,
            bins: vec!["pkg/a".into(), "pkg/b".into()],
            rename: None,
            strip_components: 1,
            os: None,
            arch: None,
            libc: None,
        };
        postprocess_archive(&ctx, &locator, &rules).unwrap();
        assert!(install.join("bin/a").is_file());
        assert!(install.join("bin/b").is_file());

        let bad_version = ToolVersion::new(backend.id(), "2.0.0");
        let bad_locator =
            github_install_locator_for_artifact(&ctx, backend.id(), &bad_version, &asset, None)
                .unwrap();
        let bad_install = bad_locator.install_root();
        std::fs::create_dir_all(bad_install.join("release/pkg")).unwrap();
        std::fs::write(bad_install.join("release/pkg/a"), b"a").unwrap();
        let bad = AssetRules {
            bins: vec!["pkg/a".into(), "pkg/missing".into()],
            ..rules.clone()
        };
        assert!(postprocess_archive(&ctx, &bad_locator, &bad).is_err());
        assert!(!bad_install.exists());
        assert_eq!(normalize_executable_name("tool", Os::Windows), "tool.exe");
        assert_eq!(
            normalize_executable_name("tool.exe", Os::Windows),
            "tool.exe"
        );
    }
}
