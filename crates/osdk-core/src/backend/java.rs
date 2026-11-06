//! Java backend: discovers JDKs across vendors via the Foojay Disco API and
//! downloads the vendor archive. Default distribution is Temurin.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::backend::{Backend, Ctx, InstallCtx};
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::{self, ArchiveKind, InstallPlan, PipelineCtx};
use crate::platform::Os;
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo, VersionSpec};

pub struct JavaBackend;

const DEFAULT_DISTRIBUTION: &str = "temurin";
const BUILTIN_TEMURIN_LTS: &[&str] = &[
    "8.0.502+7",
    "11.0.32+9",
    "17.0.20+8",
    "21.0.12+8",
    "25.0.4+7",
];

#[derive(Debug, Deserialize)]
struct DiscoResponse<T> {
    result: Vec<T>,
}

#[derive(Debug, Deserialize, Clone)]
struct Package {
    #[serde(default)]
    id: String,
    #[serde(default)]
    java_version: String,
    #[serde(default)]
    distribution_version: String,
    #[serde(default)]
    filename: String,
    #[serde(default)]
    #[allow(dead_code)] // parsed from the API; retained for schema clarity
    archive_type: String,
    #[serde(default)]
    links: PackageLinks,
    #[serde(default)]
    #[allow(dead_code)] // parsed from the API; retained for schema clarity
    distribution: String,
    // Present on the /ids/<id> detail response, not the /packages list.
    #[serde(default)]
    checksum: String,
    #[serde(default)]
    checksum_type: String,
    #[serde(default)]
    package_type: String,
    #[serde(default)]
    lib_c_type: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct PackageLinks {
    #[serde(default)]
    pkg_download_redirect: String,
}

impl JavaBackend {
    fn source_catalog_url(source: &Source) -> String {
        source
            .index_url
            .clone()
            .unwrap_or_else(|| source.download_url.clone())
    }

    fn distributions_url(source: &Source) -> String {
        let packages = Self::source_catalog_url(source);
        let root = packages
            .trim_end_matches('/')
            .strip_suffix("/packages")
            .unwrap_or(packages.trim_end_matches('/'));
        format!("{root}/distributions")
    }

    fn os_token(os: Os) -> &'static str {
        match os {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => "windows",
        }
    }

    fn arch_token(ctx: &Ctx) -> &'static str {
        use crate::platform::Arch;
        match ctx.platform.arch {
            Arch::X64 => "x64",
            Arch::Arm64 => "aarch64",
            Arch::X86 => "x86",
            Arch::Arm => "arm",
        }
    }

    fn archive_type(os: Os) -> &'static str {
        match os {
            Os::Windows => "zip",
            _ => "tar.gz",
        }
    }

    fn package_type(req_opts: &BTreeMap<String, String>) -> Result<String> {
        let value = req_opts
            .get("package-type")
            .map(String::as_str)
            .unwrap_or("jdk");
        match value {
            "jdk" | "jre" => Ok(value.into()),
            _ => Err(Error::config(format!(
                "invalid Java package type `{value}` (expected jdk|jre)"
            ))),
        }
    }

    fn libc_token(ctx: &Ctx) -> &'static str {
        match ctx.platform.libc {
            crate::platform::Libc::Musl => "musl",
            crate::platform::Libc::Glibc => "glibc",
            crate::platform::Libc::None => "none",
        }
    }

    fn packages_url(
        ctx: &Ctx,
        base_index: &str,
        distribution: &str,
        package_type: &str,
        version_filter: Option<&str>,
    ) -> String {
        Self::packages_url_with(
            ctx,
            base_index,
            distribution,
            package_type,
            version_filter,
            true,
        )
    }

    /// `latest_only` adds Foojay's `latest=available`: one package per release
    /// line, small enough for every listing. It must not accompany a `version`
    /// filter, because Foojay then ignores the filter and answers with the
    /// newest of that line -- `version=21.0.12+8` returned 21.0.12.1+1, which
    /// was installed and labelled 21.0.12+8.
    fn packages_url_with(
        ctx: &Ctx,
        base_index: &str,
        distribution: &str,
        package_type: &str,
        version_filter: Option<&str>,
        latest_only: bool,
    ) -> String {
        let os = Self::os_token(ctx.platform.os);
        let arch = Self::arch_token(ctx);
        let at = Self::archive_type(ctx.platform.os);
        let mut url = format!(
            "{base}?distribution={dist}&operating_system={os}&architecture={arch}&archive_type={at}&package_type={package_type}",
            base = base_index.trim_end_matches('/'),
            dist = distribution,
            os = os,
            arch = arch,
            at = at,
            package_type = package_type,
        );
        match version_filter {
            Some(v) => url.push_str(&format!("&version={}", v.replace('+', "%2B"))),
            None if latest_only => url.push_str("&latest=available"),
            None => {}
        }
        url
    }
}

#[async_trait]
impl Backend for JavaBackend {
    fn id(&self) -> &str {
        "java"
    }

    fn aliases(&self) -> &[&str] {
        &["jdk", "openjdk"]
    }

    fn default_sources(&self) -> Vec<Source> {
        vec![
            Source::official("foojay", "https://api.foojay.io/disco/v3.0/packages")
                .with_index("https://api.foojay.io/disco/v3.0/packages"),
        ]
    }

    fn probe_url(&self, _ctx: &Ctx, source: &Source) -> Option<String> {
        // A tiny metadata endpoint for probing.
        Some(Self::distributions_url(source))
    }

    #[cfg(feature = "install")]
    /// java version specs can carry a distribution prefix like `temurin-21`.
    async fn resolve_version(&self, ctx: &Ctx, req: &ToolRequest) -> Result<ToolVersion> {
        if req
            .options
            .contains_key(pipeline::LOCKED_ARTIFACT_URL_OPTION)
        {
            if let VersionSpec::Exact(identity) = &req.spec {
                let mut resolved = ToolVersion::new(self.id(), identity);
                resolved.options = req.options.clone();
                return Ok(resolved);
            }
        }
        // A distribution in the spec wins; otherwise `-o distribution=zulu`,
        // which used to be overwritten here with the default.
        let (distribution, spec) = requested_distribution(&req.spec, &req.options);
        let mut opts = req.options.clone();
        opts.insert("distribution".to_string(), distribution.clone());
        let package_type = Self::package_type(&opts)?;
        opts.insert("package-type".into(), package_type.clone());

        // Query packages for this distribution and select by the spec.
        let mut versions = self
            .list_for_distribution(ctx, &distribution, &package_type)
            .await?;
        // The cheap listing holds only each line's newest release, so an
        // older pin such as `21.0.5` or SDKMAN's `java=17.0.10-tem` needs the
        // full catalog. Offline there is no catalog to widen to.
        if crate::version::select_version(&spec, &versions).is_none()
            && !ctx.config.settings.offline
        {
            versions = self
                .list_for_distribution_with(ctx, &distribution, &package_type, false)
                .await?;
        }
        let chosen = crate::version::select_version(&spec, &versions).ok_or_else(|| {
            Error::VersionResolve {
                tool: self.id().to_string(),
                spec: req.spec.to_string(),
                hint: Some(format!("no {distribution} {package_type} matched")),
            }
        })?;
        let identity = install_identity(&distribution, &package_type, &chosen.version);
        let mut tv = ToolVersion::new(self.id(), identity);
        opts.insert("java-version".into(), chosen.version.clone());
        tv.options = opts;
        Ok(tv)
    }

    #[cfg(feature = "install")]
    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        self.list_for_distribution(ctx, DEFAULT_DISTRIBUTION, "jdk")
            .await
    }

    #[cfg(feature = "install")]
    async fn install_inner(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        if let Some(plan) = pipeline::locked_install_plan(self.id(), tv, true)? {
            let pctx = PipelineCtx {
                client: &ctx.client,
                dirs: &ctx.dirs,
                cas: &ctx.cas,
                link_mode: ctx.config.settings.link_mode,
                show_progress: ctx.show_progress,
                offline: ctx.config.settings.offline,
                require_checksums: ctx.config.settings.require_checksums,
            };
            pipeline::run(&plan, &pctx).await?;
            return Ok(());
        }
        let identity = parse_identity(&tv.version);
        let distribution = tv
            .options
            .get("distribution")
            .map(String::as_str)
            .or(identity.distribution)
            .unwrap_or(DEFAULT_DISTRIBUTION)
            .to_string();
        let package_type = Self::package_type(&tv.options)?;
        let java_version = tv
            .options
            .get("java-version")
            .map(String::as_str)
            .unwrap_or(identity.version);
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let (base_index, source, pkg) = self
            .package_for_version(ctx, &sources, &distribution, &package_type, java_version)
            .await?;

        let file_name = if pkg.filename.is_empty() {
            format!(
                "{}-{}.{}",
                distribution,
                java_version,
                Self::archive_type(ctx.platform.os)
            )
        } else {
            pkg.filename.clone()
        };
        let kind = ArchiveKind::from_name(&file_name)?;

        // Resolve the foojay redirect to the real vendor URL so we can add a
        // gh-proxy fallback for GitHub-hosted assets (Temurin etc.) in CN.
        let redirect = pkg.links.pkg_download_redirect.clone();
        let mut urls = Vec::new();
        if !ctx.config.settings.offline {
            if let Ok(real) = resolve_redirect(&ctx.client, &redirect).await {
                if real.contains("github.com") {
                    // Prefer a CN proxy first, then the direct GitHub URL.
                    urls.push(format!("https://gh-proxy.com/{real}"));
                    urls.push(real);
                } else {
                    urls.push(real);
                }
            }
        }
        // Always keep the foojay redirect itself as a final fallback.
        urls.push(redirect);

        // Fetch the per-id detail to get the vendor-published sha256 checksum.
        let checksum = self
            .fetch_checksum(ctx, &sources, &base_index, source.as_ref(), &pkg.id)
            .await;

        let plan = InstallPlan {
            tool: self.id().to_string(),
            version: tv.version.clone(),
            urls,
            file_name,
            kind,
            checksum,
            strip_root: true, // JDK archives wrap in jdk-<ver>/
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
        pipeline::run(&plan, &pctx).await?;
        Ok(())
    }

    fn bin_paths(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        // macOS JDK bundles nest under Contents/Home.
        let home = if ctx.platform.os == Os::Macos && root.join("Contents/Home").exists() {
            root.join("Contents/Home")
        } else {
            root
        };
        Ok(vec![home.join("bin")])
    }

    fn exec_env(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<BTreeMap<String, String>> {
        let root = ctx.dirs.install_path(self.id(), &tv.version);
        let home = if ctx.platform.os == Os::Macos && root.join("Contents/Home").exists() {
            root.join("Contents/Home")
        } else {
            root
        };
        let mut env = BTreeMap::new();
        env.insert("JAVA_HOME".to_string(), home.display().to_string());
        Ok(env)
    }

    fn bin_names(&self, ctx: &Ctx, tv: &ToolVersion) -> Result<Vec<String>> {
        let paths = self.bin_paths(ctx, tv)?;
        let discovered = crate::backend::bin_names_in_dirs(&paths);
        if discovered.is_empty() {
            if parse_identity(&tv.version).jre
                || tv.options.get("package-type").map(String::as_str) == Some("jre")
            {
                Ok(vec!["java".into(), "keytool".into()])
            } else {
                Ok(vec!["java".into(), "javac".into(), "jar".into()])
            }
        } else {
            Ok(discovered)
        }
    }

    fn idiomatic_files(&self) -> &[&str] {
        &[".java-version", ".sdkmanrc"]
    }
}

impl JavaBackend {
    fn catalog_endpoints<'a>(
        ctx: &Ctx,
        sources: &'a [Source],
    ) -> Vec<(String, Option<&'a Source>)> {
        if let Some(url) = ctx.config.settings.java_catalog_url() {
            return vec![(url, None)];
        }
        let mut endpoints = sources
            .iter()
            .map(|source| (Self::source_catalog_url(source), Some(source)))
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            endpoints.push(("https://api.foojay.io/disco/v3.0/packages".into(), None));
        }
        endpoints
    }

    async fn fetch_packages<T: serde::de::DeserializeOwned>(
        ctx: &Ctx,
        source: Option<&Source>,
        url: &str,
    ) -> Result<DiscoResponse<T>> {
        match source {
            Some(source) => http::get_cached_source_json(ctx, source, url).await,
            None => http::get_cached_json(ctx, url).await,
        }
    }

    async fn package_for_version(
        &self,
        ctx: &Ctx,
        sources: &[Source],
        distribution: &str,
        package_type: &str,
        version: &str,
    ) -> Result<(String, Option<Source>, Package)> {
        let mut last_error = None;
        for (base, source) in Self::catalog_endpoints(ctx, sources) {
            let url = Self::packages_url(ctx, &base, distribution, package_type, Some(version));
            match Self::fetch_packages::<Package>(ctx, source, &url).await {
                Ok(response) => {
                    if let Some(package) = response.result.into_iter().find(|package| {
                        // The catalog's answer has to be the version asked
                        // for; anything else would be installed under the
                        // wrong name and recorded in the lock as such.
                        java_package_is_version(package, version)
                            && !package.links.pkg_download_redirect.is_empty()
                            && (package.package_type.is_empty()
                                || package.package_type == package_type)
                            && (ctx.platform.os != Os::Linux
                                || package.lib_c_type.is_empty()
                                || package.lib_c_type == Self::libc_token(ctx))
                    }) {
                        return Ok((base, source.cloned(), package));
                    }
                    last_error = Some(Error::VersionResolve {
                        tool: self.id().to_string(),
                        spec: version.into(),
                        hint: Some(format!(
                            "no {distribution} {package_type} package for this platform at {base}"
                        )),
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| Error::NoUsableSource {
            tool: self.id().into(),
            tried: sources.len(),
        }))
    }

    async fn list_for_distribution(
        &self,
        ctx: &Ctx,
        distribution: &str,
        package_type: &str,
    ) -> Result<Vec<VersionInfo>> {
        self.list_for_distribution_with(ctx, distribution, package_type, true)
            .await
    }

    /// Every published version when `latest_only` is false (about 100 KB for
    /// Temurin on one platform), otherwise the newest of each release line.
    async fn list_for_distribution_with(
        &self,
        ctx: &Ctx,
        distribution: &str,
        package_type: &str,
        latest_only: bool,
    ) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let mut response_error = None;

        use std::collections::BTreeSet;
        let mut set: BTreeSet<String> = BTreeSet::new();
        for (base, source) in Self::catalog_endpoints(ctx, &sources) {
            let url =
                Self::packages_url_with(ctx, &base, distribution, package_type, None, latest_only);
            match Self::fetch_packages::<Package>(ctx, source, &url).await {
                Ok(response) => {
                    for package in response.result {
                        if !package.package_type.is_empty() && package.package_type != package_type
                        {
                            continue;
                        }
                        if ctx.platform.os == Os::Linux
                            && !package.lib_c_type.is_empty()
                            && package.lib_c_type != Self::libc_token(ctx)
                        {
                            continue;
                        }
                        let version = if !package.java_version.is_empty() {
                            package.java_version
                        } else {
                            package.distribution_version
                        };
                        if !version.is_empty() {
                            set.insert(version);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        source = %source.map(|source| source.id.as_str()).unwrap_or("catalog-url"),
                        error = %error,
                        "Java catalog source failed; trying the next source"
                    );
                    response_error = Some(error);
                }
            }
        }
        if distribution == DEFAULT_DISTRIBUTION {
            set.extend(BUILTIN_TEMURIN_LTS.iter().map(|value| (*value).to_string()));
        }
        if set.is_empty() {
            return Err(response_error.unwrap_or_else(|| Error::VersionResolve {
                tool: self.id().into(),
                spec: "latest".into(),
                hint: Some(format!("no {distribution} {package_type} versions")),
            }));
        }
        let mut out: Vec<VersionInfo> = set
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

    /// Fetch the vendor-published sha256 for a package id from the foojay
    /// `/ids/<id>` detail endpoint. Best-effort: returns None on any failure so
    /// installs still proceed (download failover/extraction remain the guard).
    async fn fetch_checksum(
        &self,
        ctx: &Ctx,
        sources: &[Source],
        base_index: &str,
        source: Option<&Source>,
        id: &str,
    ) -> Option<crate::pipeline::Checksum> {
        if id.is_empty() {
            return None;
        }
        let mut endpoints = vec![(base_index.to_string(), source.cloned())];
        for (base, source) in Self::catalog_endpoints(ctx, sources) {
            if !endpoints.iter().any(|(known, _)| known == &base) {
                endpoints.push((base, source.cloned()));
            }
        }
        for (base, source) in endpoints {
            // base is ".../disco/v3.0/packages"; ids is a sibling endpoint.
            let base = base.trim_end_matches('/');
            let root = base.strip_suffix("/packages").unwrap_or(base);
            let url = format!("{root}/ids/{id}");
            let Ok(resp) = Self::fetch_packages::<Package>(ctx, source.as_ref(), &url).await else {
                continue;
            };
            let Some(pkg) = resp.result.into_iter().next() else {
                continue;
            };
            if pkg.checksum.is_empty() {
                continue;
            }
            // foojay currently publishes sha256; guard in case that changes.
            if !pkg.checksum_type.is_empty() && !pkg.checksum_type.eq_ignore_ascii_case("sha256") {
                tracing::debug!(kind = %pkg.checksum_type, "unsupported java checksum type; skipping");
                continue;
            }
            return Some(crate::pipeline::Checksum {
                algo: crate::pipeline::HashAlgo::Sha256,
                hex: pkg.checksum,
            });
        }
        None
    }
}

/// Resolve a redirect URL to its final `Location` without downloading the body.
/// Uses a one-off client with redirects disabled so we can read the header.
async fn resolve_redirect(_client: &reqwest::Client, url: &str) -> Result<String> {
    let no_redirect = reqwest::Client::builder()
        .user_agent(concat!("osdk/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let resp = no_redirect.get(url).send().await?;
    if let Some(loc) = resp.headers().get(reqwest::header::LOCATION) {
        if let Ok(s) = loc.to_str() {
            return Ok(s.to_string());
        }
    }
    // Not a redirect (some mirrors serve directly); use the final URL.
    Ok(resp.url().to_string())
}

/// Whether a catalog package is exactly `version` (`21.0.5+11`), or its
/// same-core build when `version` names no build (`21.0.5`).
fn java_package_is_version(package: &Package, version: &str) -> bool {
    let actual = if package.java_version.is_empty() {
        package.distribution_version.as_str()
    } else {
        package.java_version.as_str()
    };
    if version.contains('+') {
        actual == version
    } else {
        actual == version
            || actual
                .split_once('+')
                .is_some_and(|(core, _)| core == version)
    }
}

/// How a JDK install is named: `[<distribution>-][jre-]<java version>`.
///
/// The distribution used to be left out, so `temurin-21` and `zulu-21` both
/// installed into `21.0.5+11` -- the second install found the first and called
/// itself done -- and `java = "temurin-21"` could select a Zulu install that
/// happened to carry a higher number. The default distribution keeps the bare
/// name, so existing Temurin installs and locks stay valid.
pub fn install_identity(distribution: &str, package_type: &str, version: &str) -> String {
    let mut identity = String::new();
    if distribution != DEFAULT_DISTRIBUTION {
        identity.push_str(distribution);
        identity.push('-');
    }
    if package_type == "jre" {
        identity.push_str("jre-");
    }
    identity.push_str(version);
    identity
}

/// The parts of an install identity (see [`install_identity`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaIdentity<'a> {
    /// `None` for a bare name: the default distribution, or an install made
    /// before identities carried one (see [`installed_distribution`]).
    pub distribution: Option<&'a str>,
    pub jre: bool,
    pub version: &'a str,
}

fn is_distribution_name(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_alphabetic())
        && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !word.chars().all(|c| c.is_ascii_digit())
}

pub fn parse_identity(identity: &str) -> JavaIdentity<'_> {
    let (distribution, rest) = match identity.split_once('-') {
        Some((word, rest)) if word != "jre" && is_distribution_name(word) => (Some(word), rest),
        _ => (None, identity),
    };
    let (jre, version) = match rest.strip_prefix("jre-") {
        Some(version) => (true, version),
        None => (false, rest),
    };
    JavaIdentity {
        distribution,
        jre,
        version,
    }
}

/// The distribution of an installed JDK.
///
/// A prefixed identity names it. A bare one is the default distribution --
/// unless it predates prefixed identities, when any distribution installed
/// that way. Every JDK records its vendor in `<home>/release`
/// (`IMPLEMENTOR="Azul Systems, Inc."`), which tells them apart without
/// asking anyone to reinstall.
pub fn installed_distribution(ctx: &Ctx, identity: &str) -> String {
    if let Some(distribution) = parse_identity(identity).distribution {
        return distribution.to_string();
    }
    let root = ctx.dirs.install_path("java", identity);
    [root.join("release"), root.join("Contents/Home/release")]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|release| distribution_from_release(&release))
        .unwrap_or(DEFAULT_DISTRIBUTION)
        .to_string()
}

/// Map a JDK `release` file's `IMPLEMENTOR` to a Foojay distribution name.
fn distribution_from_release(release: &str) -> Option<&'static str> {
    let implementor = release.lines().find_map(|line| {
        line.strip_prefix("IMPLEMENTOR=")
            .map(|value| value.trim().trim_matches('"').to_ascii_lowercase())
    })?;
    let distribution = if implementor.contains("adoptium") {
        "temurin"
    } else if implementor.contains("azul") {
        "zulu"
    } else if implementor.contains("amazon") {
        "corretto"
    } else if implementor.contains("bellsoft") {
        "liberica"
    } else if implementor.contains("microsoft") {
        "microsoft"
    } else if implementor.contains("sap") {
        "sap_machine"
    } else if implementor.contains("ibm") || implementor.contains("international business") {
        "semeru"
    } else if implementor.contains("graalvm") {
        "graalvm_community"
    } else if implementor.contains("alibaba") {
        "dragonwell"
    } else if implementor.contains("tencent") {
        "kona"
    } else if implementor.contains("oracle") {
        "oracle"
    } else {
        return None;
    };
    Some(distribution)
}

/// Split a configured Java spec into its distribution and version parts:
/// `zulu-21` -> (`zulu`, `21`), `21` -> (`temurin`, `21`).
pub fn split_spec(spec: &str) -> (&str, &str) {
    let (named, version) = split_named_spec(spec);
    (named.unwrap_or(DEFAULT_DISTRIBUTION), version)
}

/// [`split_spec`] without the default: `None` when the spec names no
/// distribution, so a `distribution` option can supply it.
fn split_named_spec(spec: &str) -> (Option<&str>, &str) {
    match spec.split_once('-') {
        Some((word, rest)) if word != "jre" && is_distribution_name(word) && !rest.is_empty() => {
            (Some(word), rest)
        }
        _ => (None, spec),
    }
}

/// A `java = { version = "21", distribution = "zulu" }` option.
fn configured_option(ctx: &Ctx, key: &str) -> Option<String> {
    ctx.config
        .tool_configs
        .get("java")
        .and_then(|entry| entry.to_request_options().get(key).cloned())
}

/// The installed JDK `spec` selects: one of the requested distribution (the
/// default when the spec names none), by version as any other tool.
///
/// A JRE install is selected only by its full identity, as before.
pub fn select_installed(
    ctx: &Ctx,
    spec: &str,
    is_range: bool,
    installed: &[String],
) -> Option<String> {
    if parse_identity(spec).jre {
        return installed
            .iter()
            .any(|identity| identity == spec)
            .then(|| spec.to_string());
    }
    let (named, version) = split_named_spec(spec);
    // The configured options select as they install: `distribution` when the
    // spec names none, and `package-type = "jre"`.
    let distribution = named
        .map(str::to_string)
        .or_else(|| configured_option(ctx, "distribution"))
        .unwrap_or_else(|| DEFAULT_DISTRIBUTION.to_string());
    let jre = configured_option(ctx, "package-type").as_deref() == Some("jre");
    let mut candidates: Vec<(&String, &str)> = installed
        .iter()
        .filter_map(|identity| {
            let parsed = parse_identity(identity);
            (parsed.jre == jre && installed_distribution(ctx, identity) == distribution)
                .then_some((identity, parsed.version))
        })
        .collect();
    // Ascending by version, as the selector expects. The identities do not
    // sort that way once a legacy bare name sits beside a prefixed one.
    candidates.sort_by(|a, b| crate::version::compare_version_strings(a.1, b.1));
    let parsed = if is_range {
        VersionSpec::parse_range(version).ok()?
    } else {
        VersionSpec::parse(version)
    };
    if let VersionSpec::Exact(exact) = &parsed {
        if let Some((identity, _)) = candidates.iter().find(|(_, v)| v == exact) {
            return Some((*identity).clone());
        }
    }
    let infos: Vec<VersionInfo> = candidates
        .iter()
        .map(|(_, version)| VersionInfo::stable(*version))
        .collect();
    let chosen = crate::version::select_version(&parsed, &infos)?;
    candidates
        .iter()
        .find(|(_, version)| *version == chosen.version)
        .map(|(identity, _)| (*identity).clone())
}

/// A lock written before identities carried the distribution records the
/// bare version for a `zulu-21` request. Qualify it with the request's
/// distribution so selection looks among that distribution's installs.
pub fn qualify_locked_version(request: &str, version: &str) -> String {
    let (distribution, _) = split_spec(request);
    if distribution == DEFAULT_DISTRIBUTION || parse_identity(version).distribution.is_some() {
        version.to_string()
    } else {
        format!("{distribution}-{version}")
    }
}

/// The distribution a request installs: named in the spec, else the
/// `distribution` option, else the default.
fn requested_distribution(
    spec: &VersionSpec,
    options: &BTreeMap<String, String>,
) -> (String, VersionSpec) {
    let (named, spec) = split_distribution(spec);
    let distribution = named
        .or_else(|| options.get("distribution").cloned())
        .unwrap_or_else(|| DEFAULT_DISTRIBUTION.to_string());
    (distribution, spec)
}

/// Split a java spec like `temurin-21` or `zulu-17.0.1` into (distribution,
/// version-spec). Plain `21`/`lts`/`latest` name no distribution.
fn split_distribution(spec: &VersionSpec) -> (Option<String>, VersionSpec) {
    if let VersionSpec::Prefix(p) | VersionSpec::Exact(p) = spec {
        if let Some((dist, ver)) = p.split_once('-') {
            if dist
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
            {
                return (Some(dist.to_string()), VersionSpec::parse(ver));
            }
        }
    }
    (None, spec.clone())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, Settings, SourcesConfig};
    use crate::dirs::Dirs;
    use crate::platform::{Arch, Libc, Platform};
    use crate::store::Cas;
    use std::collections::BTreeMap;

    #[test]
    fn install_identities_carry_a_non_default_distribution() {
        assert_eq!(install_identity("temurin", "jdk", "21.0.5+11"), "21.0.5+11");
        assert_eq!(
            install_identity("temurin", "jre", "21.0.5+11"),
            "jre-21.0.5+11"
        );
        assert_eq!(
            install_identity("zulu", "jdk", "21.0.5+11"),
            "zulu-21.0.5+11"
        );
        assert_eq!(
            install_identity("sap_machine", "jre", "17.0.10"),
            "sap_machine-jre-17.0.10"
        );
        for (identity, distribution, jre, version) in [
            ("21.0.5+11", None, false, "21.0.5+11"),
            ("jre-21.0.5+11", None, true, "21.0.5+11"),
            ("zulu-21.0.5+11", Some("zulu"), false, "21.0.5+11"),
            (
                "sap_machine-jre-17.0.10",
                Some("sap_machine"),
                true,
                "17.0.10",
            ),
        ] {
            assert_eq!(
                parse_identity(identity),
                JavaIdentity {
                    distribution,
                    jre,
                    version
                },
                "{identity}"
            );
        }
        assert_eq!(split_spec("zulu-21"), ("zulu", "21"));
        assert_eq!(split_spec("21"), ("temurin", "21"));
        assert_eq!(split_spec("lts"), ("temurin", "lts"));
    }

    /// `java = "temurin-21"` must not run a Zulu install that carries a higher
    /// number, and installs made before identities named their distribution
    /// are told apart by the JDK's own `release` file.
    #[test]
    fn selection_stays_within_the_requested_distribution() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = offline_ctx(temp.path());
        let installed: Vec<String> = ["21.0.5+11", "zulu-21.0.9+10", "21.0.7+6", "jre-21.0.8+9"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for identity in &installed {
            std::fs::create_dir_all(ctx.dirs.install_path("java", identity)).unwrap();
        }
        // A pre-identity Zulu install, named by version only.
        std::fs::write(
            ctx.dirs.install_path("java", "21.0.7+6").join("release"),
            "JAVA_VERSION=\"21.0.7\"
IMPLEMENTOR=\"Azul Systems, Inc.\"
",
        )
        .unwrap();
        let pick = |spec: &str| select_installed(&ctx, spec, false, &installed);
        assert_eq!(pick("temurin-21").as_deref(), Some("21.0.5+11"));
        assert_eq!(pick("21").as_deref(), Some("21.0.5+11"));
        assert_eq!(pick("zulu-21").as_deref(), Some("zulu-21.0.9+10"));
        assert_eq!(pick("zulu-21.0.7").as_deref(), Some("21.0.7+6"));
        assert_eq!(pick("21.0.7+6"), None, "that bare name is a Zulu install");
        assert_eq!(pick("corretto-21"), None);
        assert_eq!(pick("jre-21.0.8+9").as_deref(), Some("jre-21.0.8+9"));
        assert_eq!(installed_distribution(&ctx, "21.0.7+6"), "zulu");
        assert_eq!(installed_distribution(&ctx, "21.0.5+11"), "temurin");
    }

    #[test]
    fn a_lock_from_before_identities_is_qualified_by_its_request() {
        assert_eq!(
            qualify_locked_version("zulu-21", "21.0.5+11"),
            "zulu-21.0.5+11"
        );
        assert_eq!(
            qualify_locked_version("zulu-21", "zulu-21.0.5+11"),
            "zulu-21.0.5+11"
        );
        assert_eq!(qualify_locked_version("21", "21.0.5+11"), "21.0.5+11");
        assert_eq!(
            qualify_locked_version("temurin-21", "21.0.5+11"),
            "21.0.5+11"
        );
    }

    #[test]
    fn distribution_split() {
        let (d, v) = split_distribution(&VersionSpec::parse("temurin-21"));
        assert_eq!(d.as_deref(), Some("temurin"));
        assert_eq!(v, VersionSpec::Prefix("21".into()));

        // No distribution named: the `distribution` option or the default.
        let (d, v) = split_distribution(&VersionSpec::parse("21"));
        assert_eq!(d, None);
        assert_eq!(v, VersionSpec::Prefix("21".into()));

        let (d, _) = split_distribution(&VersionSpec::parse("zulu-17.0.1"));
        assert_eq!(d.as_deref(), Some("zulu"));
    }

    fn offline_ctx(root: &std::path::Path) -> Ctx {
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
                settings: Settings {
                    offline: true,
                    ..Default::default()
                },
                sources: SourcesConfig::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                global_tools: Default::default(),
                global_tool_configs: Default::default(),
                tool_origins: Default::default(),
                tool_aliases: Default::default(),
                project_config_path: None,
                excluded_tools: Default::default(),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    #[tokio::test]
    async fn empty_cache_offline_resolves_builtin_lts_jdk_and_jre() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = offline_ctx(temp.path());

        let jdk = JavaBackend
            .resolve_version(&ctx, &ToolRequest::parse("java@21").unwrap())
            .await
            .unwrap();
        assert_eq!(jdk.version, "21.0.12+8");
        assert_eq!(jdk.options["package-type"], "jdk");

        let mut jre_request = ToolRequest::parse("java@21").unwrap();
        jre_request
            .options
            .insert("package-type".into(), "jre".into());
        let jre = JavaBackend
            .resolve_version(&ctx, &jre_request)
            .await
            .unwrap();
        assert_eq!(jre.version, "jre-21.0.12+8");
        assert_eq!(jre.options["java-version"], "21.0.12+8");
        assert_ne!(jdk.version, jre.version);
    }

    #[tokio::test]
    async fn version_without_build_number_matches_same_core_temurin() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = offline_ctx(temp.path());

        // `21.0.12` (no build number) must resolve to the same-core
        // `21.0.12+8`, not error and not jump to the four-part PSU.
        let same_core = JavaBackend
            .resolve_version(&ctx, &ToolRequest::parse("java@21.0.12").unwrap())
            .await
            .unwrap();
        assert_eq!(same_core.version, "21.0.12+8");

        // A fully-qualified version with build number keeps working.
        let exact = JavaBackend
            .resolve_version(&ctx, &ToolRequest::parse("java@21.0.12+8").unwrap())
            .await
            .unwrap();
        assert_eq!(exact.version, "21.0.12+8");
    }

    #[test]
    fn package_urls_and_filtering_include_runtime_type_and_libc() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = offline_ctx(temp.path());
        let url = JavaBackend::packages_url(
            &ctx,
            "https://example.test/packages",
            "temurin",
            "jre",
            Some("21"),
        );
        assert!(url.contains("package_type=jre"));
        assert!(url.contains("version=21"));
        assert_eq!(JavaBackend::libc_token(&ctx), "glibc");

        // With `latest=available` Foojay ignores `version` and returns the
        // line's newest build, so an exact query must never carry it.
        let exact = JavaBackend::packages_url(
            &ctx,
            "https://x.test/p",
            "temurin",
            "jdk",
            Some("21.0.12+8"),
        );
        assert!(exact.ends_with("&version=21.0.12%2B8"), "{exact}");
        assert!(!exact.contains("latest="), "{exact}");
        let listing = JavaBackend::packages_url(&ctx, "https://x.test/p", "temurin", "jdk", None);
        assert!(listing.contains("&latest=available"), "{listing}");
        let full =
            JavaBackend::packages_url_with(&ctx, "https://x.test/p", "temurin", "jdk", None, false);
        assert!(!full.contains("latest="), "{full}");
    }

    #[test]
    fn a_catalog_package_must_be_the_requested_version() {
        let package = |java_version: &str| -> Package {
            serde_json::from_value(serde_json::json!({ "java_version": java_version })).unwrap()
        };
        assert!(java_package_is_version(&package("21.0.12+8"), "21.0.12+8"));
        assert!(java_package_is_version(&package("21.0.12+8"), "21.0.12"));
        // The respin Foojay answered with must not pass for the release.
        assert!(!java_package_is_version(
            &package("21.0.12.1+1"),
            "21.0.12+8"
        ));
        assert!(!java_package_is_version(&package("21.0.12.1+1"), "21.0.12"));
        assert!(!java_package_is_version(&package("21.0.5+11"), "21.0.5+12"));
    }

    #[tokio::test]
    async fn exact_package_lookup_falls_through_a_stale_catalog() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8(request).unwrap();
                let body = if request.starts_with("GET /preferred?") {
                    r#"{"result":[]}"#
                } else {
                    r#"{"result":[{"id":"jdk-21","java_version":"21.0.12+8","filename":"jdk.tar.gz","package_type":"jdk","lib_c_type":"glibc","links":{"pkg_download_redirect":"https://download.example/jdk.tar.gz"}}]}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let temporary = tempfile::tempdir().unwrap();
        let mut ctx = offline_ctx(temporary.path());
        ctx.config.settings.offline = false;
        let sources = vec![
            Source::mirror("preferred", &format!("http://{address}/preferred"), 0),
            Source::mirror("fallback", &format!("http://{address}/fallback"), 10),
        ];

        let (base, source, package) = JavaBackend
            .package_for_version(&ctx, &sources, "temurin", "jdk", "21.0.12+8")
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(base, format!("http://{address}/fallback"));
        assert_eq!(source.unwrap().id, "fallback");
        assert_eq!(package.id, "jdk-21");
    }

    /// `-o distribution=zulu` is documented, but resolution replaced it with
    /// the default whenever the spec named no distribution, so the request
    /// installed Temurin.
    #[test]
    fn the_distribution_option_applies_when_the_spec_names_none() {
        let zulu = BTreeMap::from([("distribution".to_string(), "zulu".to_string())]);
        let (distribution, spec) = requested_distribution(&VersionSpec::parse("21"), &zulu);
        assert_eq!(distribution, "zulu");
        assert_eq!(spec, VersionSpec::Prefix("21".into()));
        assert_eq!(
            install_identity(&distribution, "jdk", "21.0.9+10"),
            "zulu-21.0.9+10"
        );
        // The spec's own distribution wins over the option.
        let (distribution, _) = requested_distribution(&VersionSpec::parse("corretto-21"), &zulu);
        assert_eq!(distribution, "corretto");
        let (distribution, _) = requested_distribution(&VersionSpec::parse("21"), &BTreeMap::new());
        assert_eq!(distribution, "temurin");
    }

    #[tokio::test]
    async fn locked_java_archive_installs_offline_without_foojay() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = offline_ctx(temp.path());
        let archive = temp.path().join("java-fixture.tar.gz");
        let file = std::fs::File::create(&archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let contents = b"#!/bin/sh\nexit 0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "jdk/bin/java", &contents[..])
            .unwrap();
        builder.finish().unwrap();
        drop(builder);
        let checksum =
            pipeline::verify::hash_file(&archive, crate::pipeline::HashAlgo::Sha256).unwrap();
        let cached = pipeline::artifact_cache_path(
            &ctx.dirs,
            "java",
            "jre-21.0.12+8",
            "java-fixture.tar.gz",
        )
        .unwrap();
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::copy(&archive, &cached).unwrap();
        let mut version = ToolVersion::new("java", "jre-21.0.12+8");
        version.options = BTreeMap::from([
            ("package-type".into(), "jre".into()),
            ("java-version".into(), "21.0.12+8".into()),
            (
                pipeline::LOCKED_ARTIFACT_URL_OPTION.into(),
                "https://invalid.example/java-fixture.tar.gz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_FILE_OPTION.into(),
                "java-fixture.tar.gz".into(),
            ),
            (
                pipeline::LOCKED_ARTIFACT_CHECKSUM_OPTION.into(),
                format!("sha256:{checksum}"),
            ),
        ]);
        JavaBackend
            .install(&InstallCtx { ctx: &ctx }, &version)
            .await
            .unwrap();
        assert!(ctx
            .dirs
            .install_path("java", "jre-21.0.12+8")
            .join("bin/java")
            .is_file());
    }
}
