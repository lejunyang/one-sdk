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

    /// The distribution to use, from request options or the default.
    fn distribution(req_opts: &BTreeMap<String, String>) -> String {
        req_opts
            .get("distribution")
            .cloned()
            .unwrap_or_else(|| DEFAULT_DISTRIBUTION.to_string())
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
        let os = Self::os_token(ctx.platform.os);
        let arch = Self::arch_token(ctx);
        let at = Self::archive_type(ctx.platform.os);
        let mut url = format!(
            "{base}?distribution={dist}&operating_system={os}&architecture={arch}&archive_type={at}&package_type={package_type}&latest=available",
            base = base_index.trim_end_matches('/'),
            dist = distribution,
            os = os,
            arch = arch,
            at = at,
            package_type = package_type,
        );
        if let Some(v) = version_filter {
            url.push_str(&format!("&version={v}"));
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

    fn probe_url(&self, _ctx: &Ctx, _source: &Source) -> Option<String> {
        // A tiny metadata endpoint for probing.
        Some("https://api.foojay.io/disco/v3.0/distributions".to_string())
    }

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
        let (distribution, spec) = split_distribution(&req.spec);
        let mut opts = req.options.clone();
        opts.insert("distribution".to_string(), distribution.clone());
        let package_type = Self::package_type(&opts)?;
        opts.insert("package-type".into(), package_type.clone());

        // Query packages for this distribution and select by the spec.
        let versions = self
            .list_for_distribution(ctx, &distribution, &package_type)
            .await?;
        let chosen = crate::version::select_version(&spec, &versions).ok_or_else(|| {
            Error::VersionResolve {
                tool: self.id().to_string(),
                spec: req.spec.to_string(),
                hint: Some(format!("no {distribution} {package_type} matched")),
            }
        })?;
        let identity = if package_type == "jre" {
            format!("jre-{}", chosen.version)
        } else {
            chosen.version.clone()
        };
        let mut tv = ToolVersion::new(self.id(), identity);
        opts.insert("java-version".into(), chosen.version.clone());
        tv.options = opts;
        Ok(tv)
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        self.list_for_distribution(ctx, DEFAULT_DISTRIBUTION, "jdk")
            .await
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
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
        let distribution = Self::distribution(&tv.options);
        let package_type = Self::package_type(&tv.options)?;
        let java_version = tv
            .options
            .get("java-version")
            .map(String::as_str)
            .unwrap_or_else(|| tv.version.strip_prefix("jre-").unwrap_or(&tv.version));
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let base_index = ctx
            .config
            .settings
            .java
            .catalog_url
            .clone()
            .or_else(|| sources.first().map(|source| source.download_url.clone()))
            .unwrap_or_else(|| "https://api.foojay.io/disco/v3.0/packages".to_string());

        // Query the exact package for this version.
        let url = Self::packages_url(
            ctx,
            &base_index,
            &distribution,
            &package_type,
            Some(java_version),
        );
        let resp: DiscoResponse<Package> = http::get_cached_json(ctx, &url).await?;
        let pkg = resp
            .result
            .into_iter()
            .find(|p| {
                !p.links.pkg_download_redirect.is_empty()
                    && (p.package_type.is_empty() || p.package_type == package_type)
                    && (ctx.platform.os != Os::Linux
                        || p.lib_c_type.is_empty()
                        || p.lib_c_type == Self::libc_token(ctx))
            })
            .ok_or_else(|| Error::VersionResolve {
                tool: self.id().to_string(),
                spec: java_version.into(),
                hint: Some(format!(
                    "no {distribution} {package_type} package for this platform"
                )),
            })?;

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
        let checksum = self.fetch_checksum(ctx, &base_index, &pkg.id).await;

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
            if tv.version.starts_with("jre-")
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
    async fn list_for_distribution(
        &self,
        ctx: &Ctx,
        distribution: &str,
        package_type: &str,
    ) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let base_index = ctx
            .config
            .settings
            .java
            .catalog_url
            .clone()
            .or_else(|| sources.first().map(|source| source.download_url.clone()))
            .unwrap_or_else(|| "https://api.foojay.io/disco/v3.0/packages".to_string());
        let url = Self::packages_url(ctx, &base_index, distribution, package_type, None);
        let response = http::get_cached_json::<DiscoResponse<Package>>(ctx, &url).await;
        let mut response_error = None;

        use std::collections::BTreeSet;
        let mut set: BTreeSet<String> = BTreeSet::new();
        match response {
            Ok(response) => {
                for package in response.result {
                    if !package.package_type.is_empty() && package.package_type != package_type {
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
            Err(error) => response_error = Some(error),
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
        base_index: &str,
        id: &str,
    ) -> Option<crate::pipeline::Checksum> {
        if id.is_empty() {
            return None;
        }
        // base_index is ".../disco/v3.0/packages"; the ids endpoint is a sibling.
        let base = base_index.trim_end_matches('/');
        let root = base.strip_suffix("/packages").unwrap_or(base);
        let url = format!("{}/ids/{}", root, id);
        let resp: DiscoResponse<Package> = http::get_cached_json(ctx, &url).await.ok()?;
        let pkg = resp.result.into_iter().next()?;
        if pkg.checksum.is_empty() {
            return None;
        }
        // foojay currently publishes sha256; guard in case that changes.
        if !pkg.checksum_type.is_empty() && !pkg.checksum_type.eq_ignore_ascii_case("sha256") {
            tracing::debug!(kind = %pkg.checksum_type, "unsupported java checksum type; skipping");
            return None;
        }
        Some(crate::pipeline::Checksum {
            algo: crate::pipeline::HashAlgo::Sha256,
            hex: pkg.checksum,
        })
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

/// Split a java spec like `temurin-21` or `zulu-17.0.1` into (distribution,
/// version-spec). Plain `21`/`lts`/`latest` use the default distribution.
fn split_distribution(spec: &VersionSpec) -> (String, VersionSpec) {
    if let VersionSpec::Prefix(p) | VersionSpec::Exact(p) = spec {
        if let Some((dist, ver)) = p.split_once('-') {
            if dist
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
            {
                return (dist.to_string(), VersionSpec::parse(ver));
            }
        }
    }
    (DEFAULT_DISTRIBUTION.to_string(), spec.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, Settings, SourcesConfig};
    use crate::dirs::Dirs;
    use crate::platform::{Arch, Libc, Platform};
    use crate::store::Cas;
    use std::collections::BTreeMap;

    #[test]
    fn distribution_split() {
        let (d, v) = split_distribution(&VersionSpec::parse("temurin-21"));
        assert_eq!(d, "temurin");
        assert_eq!(v, VersionSpec::Prefix("21".into()));

        let (d, v) = split_distribution(&VersionSpec::parse("21"));
        assert_eq!(d, "temurin");
        assert_eq!(v, VersionSpec::Prefix("21".into()));

        let (d, _) = split_distribution(&VersionSpec::parse("zulu-17.0.1"));
        assert_eq!(d, "zulu");
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
                aliases: Default::default(),
                project_config_path: None,
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
        );
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
