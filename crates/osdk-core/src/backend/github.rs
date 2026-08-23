//! Generic GitHub-release backend, addressed as `github:owner/repo`.
//!
//! Downloads a release asset matching the host platform and installs it. Two
//! asset shapes are handled: archives (tar.*/zip → extracted) and bare binaries
//! (installed directly into `bin/`). Version specs map to release tags
//! (`latest` → the latest release). Mirrors: a CN GitHub proxy fallback.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::{Arch, Os};
use crate::source::Source;
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
        let rest = id.strip_prefix("github:")?;
        let mut components = rest.split('/');
        let owner = components.next()?;
        let repo = components.next()?;
        if components.next().is_some() {
            return None;
        }
        let owner = owner.trim();
        let repo = repo.trim().trim_end_matches(".git");
        if !valid_repository_component(owner) || !valid_repository_component(repo) {
            return None;
        }
        Some(GithubBackend {
            id: id.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
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
        Ok(AssetRules {
            regex,
            template: options.get("asset-template").cloned(),
            bins,
            rename: options.get("rename").cloned(),
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
                    || !matches!(
                        reqwest::Url::parse(&asset.url)
                            .ok()
                            .map(|url| url.scheme().to_string())
                            .as_deref(),
                        Some("http" | "https")
                    )
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
        let rules = Self::rules(&tv.options)?;
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let attestation = self.attestation(ctx, &sources);
        if let Some(artifact) = pipeline::locked_artifact(tv)? {
            let urls = http::github_url_candidates(&sources, &artifact.url);
            if let Ok(kind) = ArchiveKind::from_name(&artifact.file_name) {
                let plan = InstallPlan {
                    tool: self.id().to_string(),
                    version: tv.version.clone(),
                    urls,
                    file_name: artifact.file_name,
                    kind,
                    checksum: artifact
                        .checksum
                        .as_deref()
                        .map(pipeline::parse_checksum)
                        .transpose()?,
                    strip_root: rules.strip_components == 0,
                    subdir: tv.options.get("catalog-subdir").map(PathBuf::from),
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
                pipeline::run_with_attestation(&plan, &pctx, attestation.as_ref()).await?;
                postprocess_archive(ctx, self, &tv.version, &rules)?;
            } else {
                let checksum = artifact
                    .checksum
                    .as_deref()
                    .map(pipeline::parse_checksum)
                    .transpose()?;
                let exe_name = normalize_executable_name(
                    rules.rename.as_deref().unwrap_or(&self.repo),
                    ctx.platform.os,
                );
                pipeline::install_single_binary(
                    &ctx.client,
                    &ctx.dirs,
                    self.id(),
                    &tv.version,
                    &urls,
                    exe_name.trim_end_matches(ctx.platform.os.exe_suffix()),
                    &artifact.file_name,
                    ctx.platform.os,
                    checksum.as_ref(),
                    ctx.show_progress,
                    ctx.config.settings.offline,
                    ctx.config.settings.require_checksums,
                    attestation.as_ref(),
                )
                .await?;
            }
            return Ok(());
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
                    .filter(|asset| static_asset_matches(asset, ctx, &rules))
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
                        .collect::<std::collections::BTreeMap<_, _>>(),
                )
            } else {
                let release = self.release_for_tag(ctx, &sources, &tv.version).await?;
                (release.assets, std::collections::BTreeMap::new())
            };

        let asset = select_asset(self, &assets, &tv.version, ctx, &rules)?;

        let urls = http::github_url_candidates(&sources, &asset.browser_download_url);

        // Checksum discovery, strongest first:
        // 1. a minisign-signed checksums manifest (trusted key) — the manifest's
        //    signature is verified before its hashes are trusted;
        // 2. per-asset sidecar / unsigned shared manifest.
        let mut checksum = static_checksums
            .get(&asset.name)
            .map(|value| pipeline::parse_checksum(value))
            .transpose()?;
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
                    Ok(Some(cs)) => {
                        tracing::info!(source = %self.id, "{}", crate::i18n::tr("log.signature_verified"));
                        checksum = Some(cs);
                        break;
                    }
                    Ok(None) => {}
                    // Invalid signature is a hard failure — do not silently proceed.
                    Err(e) => return Err(e),
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

        // Archive vs bare binary.
        match ArchiveKind::from_name(&asset.name) {
            Ok(kind) => {
                let plan = InstallPlan {
                    tool: self.id().to_string(),
                    version: tv.version.clone(),
                    urls,
                    file_name: asset.name.clone(),
                    kind,
                    checksum,
                    // Some archives have a top dir, some don't; strip only when a
                    // single root dir is present (extract handles the no-op).
                    strip_root: rules.strip_components == 0,
                    subdir: None,
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
                pipeline::run_with_attestation(&plan, &pctx, attestation.as_ref()).await?;
                postprocess_archive(ctx, self, &tv.version, &rules)?;
            }
            Err(_) => {
                // Treat as a bare executable named after the repo.
                let exe_name = normalize_executable_name(
                    rules.rename.as_deref().unwrap_or(&self.repo),
                    ctx.platform.os,
                );
                pipeline::install_single_binary(
                    &ctx.client,
                    &ctx.dirs,
                    self.id(),
                    &tv.version,
                    &urls,
                    exe_name.trim_end_matches(ctx.platform.os.exe_suffix()),
                    &asset.name,
                    ctx.platform.os,
                    checksum.as_ref(),
                    ctx.show_progress,
                    ctx.config.settings.offline,
                    ctx.config.settings.require_checksums,
                    attestation.as_ref(),
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

fn postprocess_archive(
    ctx: &Ctx,
    backend: &GithubBackend,
    version: &str,
    rules: &AssetRules,
) -> Result<()> {
    if rules.bins.is_empty() && rules.rename.is_none() && rules.strip_components == 0 {
        return Ok(());
    }
    let install = ctx.dirs.install_path(backend.id(), version);
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
                assert!(!String::from_utf8(request)
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains("authorization:"));
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
        let cached = pipeline::artifact_cache_path(&ctx.dirs, backend.id(), "1.2.3", file_name);
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::copy(&archive, &cached).unwrap();
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
        backend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap();
        let install = ctx.dirs.install_path(backend.id(), "1.2.3");
        assert!(install.join("bin/a").is_file());
        assert!(install.join("bin/b").is_file());
        assert!(install.join(".osdk-complete").is_file());
    }

    #[test]
    fn postprocess_is_atomic_for_multiple_binaries_and_windows_names() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let backend = GithubBackend::from_id("github:example/tool").unwrap();
        let install = ctx.dirs.install_path(backend.id(), "1.0.0");
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
        postprocess_archive(&ctx, &backend, "1.0.0", &rules).unwrap();
        assert!(install.join("bin/a").is_file());
        assert!(install.join("bin/b").is_file());

        let bad_install = ctx.dirs.install_path(backend.id(), "2.0.0");
        std::fs::create_dir_all(bad_install.join("release/pkg")).unwrap();
        std::fs::write(bad_install.join("release/pkg/a"), b"a").unwrap();
        let bad = AssetRules {
            bins: vec!["pkg/a".into(), "pkg/missing".into()],
            ..rules.clone()
        };
        assert!(postprocess_archive(&ctx, &backend, "2.0.0", &bad).is_err());
        assert!(!bad_install.exists());
        assert_eq!(normalize_executable_name("tool", Os::Windows), "tool.exe");
        assert_eq!(
            normalize_executable_name("tool.exe", Os::Windows),
            "tool.exe"
        );
    }
}
