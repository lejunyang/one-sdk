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

    fn packages_url(
        ctx: &Ctx,
        base_index: &str,
        distribution: &str,
        version_filter: Option<&str>,
    ) -> String {
        let os = Self::os_token(ctx.platform.os);
        let arch = Self::arch_token(ctx);
        let at = Self::archive_type(ctx.platform.os);
        let mut url = format!(
            "{base}?distribution={dist}&operating_system={os}&architecture={arch}&archive_type={at}&package_type=jdk&latest=available",
            base = base_index.trim_end_matches('/'),
            dist = distribution,
            os = os,
            arch = arch,
            at = at,
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
        let (distribution, spec) = split_distribution(&req.spec);
        let mut opts = req.options.clone();
        opts.insert("distribution".to_string(), distribution.clone());

        // Query packages for this distribution and select by the spec.
        let versions = self.list_for_distribution(ctx, &distribution).await?;
        let chosen = crate::version::select_version(&spec, &versions).ok_or_else(|| {
            Error::VersionResolve {
                tool: self.id().to_string(),
                spec: req.spec.to_string(),
                hint: Some(format!("no {distribution} JDK matched")),
            }
        })?;
        let mut tv = ToolVersion::new(self.id(), chosen.version.clone());
        tv.options = opts;
        Ok(tv)
    }

    async fn list_remote_versions(&self, ctx: &Ctx) -> Result<Vec<VersionInfo>> {
        self.list_for_distribution(ctx, DEFAULT_DISTRIBUTION).await
    }

    async fn install(&self, ictx: &InstallCtx<'_>, tv: &ToolVersion) -> Result<()> {
        let ctx = ictx.ctx;
        let distribution = Self::distribution(&tv.options);
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let base_index = sources
            .first()
            .map(|s| s.download_url.clone())
            .unwrap_or_else(|| "https://api.foojay.io/disco/v3.0/packages".to_string());

        // Query the exact package for this version.
        let url = Self::packages_url(ctx, &base_index, &distribution, Some(&tv.version));
        let resp: DiscoResponse<Package> = http::get_json(&ctx.client, &url).await?;
        let pkg = resp
            .result
            .into_iter()
            .find(|p| !p.links.pkg_download_redirect.is_empty())
            .ok_or_else(|| Error::VersionResolve {
                tool: self.id().to_string(),
                spec: tv.version.clone(),
                hint: Some(format!("no {distribution} package for this platform")),
            })?;

        let file_name = if pkg.filename.is_empty() {
            format!(
                "{}-{}.{}",
                distribution,
                tv.version,
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
        if let Ok(real) = resolve_redirect(&ctx.client, &redirect).await {
            if real.contains("github.com") {
                // Prefer a CN proxy first, then the direct GitHub URL.
                urls.push(format!("https://gh-proxy.com/{real}"));
                urls.push(real);
            } else {
                urls.push(real);
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
            Ok(vec!["java".into(), "javac".into(), "jar".into()])
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
    ) -> Result<Vec<VersionInfo>> {
        let sources = crate::source::select::ranked_source_list(ctx, self).await?;
        let base_index = sources
            .first()
            .map(|s| s.download_url.clone())
            .unwrap_or_else(|| "https://api.foojay.io/disco/v3.0/packages".to_string());
        let url = Self::packages_url(ctx, &base_index, distribution, None);
        let resp: DiscoResponse<Package> = http::get_json(&ctx.client, &url).await?;

        use std::collections::BTreeSet;
        let mut set: BTreeSet<String> = BTreeSet::new();
        for p in resp.result {
            let v = if !p.java_version.is_empty() {
                p.java_version
            } else {
                p.distribution_version
            };
            if !v.is_empty() {
                set.insert(v);
            }
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
        let resp: DiscoResponse<Package> = http::get_json(&ctx.client, &url).await.ok()?;
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
    use super::*;

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
}
