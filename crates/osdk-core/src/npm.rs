//! Minimal npm registry helpers: resolve a package version's tarball URL +
//! Subresource Integrity (SRI), used by the pnpm/yarn backends to install
//! verified artifacts from the registry (mirror-friendly, first-party checksum).

use serde::Deserialize;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::http;
use crate::pipeline::Checksum;
use crate::source::Source;
use crate::version::{ToolRequest, ToolVersion, VersionInfo, VersionSpec};

#[derive(Debug, Deserialize)]
struct VersionDoc {
    #[serde(default)]
    dist: Dist,
}

#[derive(Debug, Deserialize, Default)]
struct Dist {
    #[serde(default)]
    tarball: String,
    #[serde(default)]
    integrity: String,
    #[serde(default)]
    shasum: String,
}

/// Resolved distribution for one package version.
#[derive(Debug, Clone)]
pub struct NpmDist {
    pub urls: Vec<String>,
    pub checksum: Option<Checksum>,
}

pub struct NpmVersions {
    pub versions: Vec<String>,
    pub dist_tags: std::collections::BTreeMap<String, String>,
}

/// Fetch the tarball URL + checksum for `package@version` (e.g. `yarn`,
/// `@pnpm/linux-x64`). Tries each selected source and retains every returned
/// tarball URL for download failover. The checksum comes from the first
/// parseable SRI `integrity` value (sha512/sha256), falling back to the legacy
/// `shasum` (sha1, unsupported by our verifier -> None).
pub async fn resolve_dist(
    ctx: &Ctx,
    sources: &[Source],
    package: &str,
    version: &str,
) -> Result<NpmDist> {
    let mut last_err: Option<Error> = None;
    let mut urls = Vec::new();
    let mut checksum: Option<Checksum> = None;
    let mut checksum_source: Option<String> = None;
    for source in sources {
        let url = package_url(&source.download_url, package, Some(version));
        match http::get_cached_source_json::<VersionDoc>(ctx, source, &url).await {
            Ok(doc) => {
                if doc.dist.tarball.is_empty() {
                    last_err = Some(Error::other(format!("no tarball for {package}@{version}")));
                    continue;
                }
                if !urls.iter().any(|url| url == &doc.dist.tarball) {
                    urls.push(doc.dist.tarball);
                }
                let source_checksum = crate::pipeline::verify::parse_sri(&doc.dist.integrity);
                let has_source_checksum = source_checksum.is_some();
                if let Some(source_checksum) = source_checksum {
                    if let Some(expected) = &checksum {
                        if expected.algo != source_checksum.algo
                            || expected.hex != source_checksum.hex
                        {
                            return Err(Error::other(format!(
                                "npm registry integrity mismatch for {package}@{version}: {} disagrees with {}",
                                source.id,
                                checksum_source.as_deref().unwrap_or("another source")
                            )));
                        }
                    } else {
                        checksum_source = Some(source.id.clone());
                        checksum = Some(source_checksum);
                    }
                }
                if !has_source_checksum && !doc.dist.shasum.is_empty() {
                    tracing::debug!(
                        package,
                        source = %source.id,
                        "npm dist has only sha1 shasum; skipping verification"
                    );
                }
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    if urls.is_empty() {
        Err(last_err.unwrap_or_else(|| Error::other(format!("cannot resolve {package}@{version}"))))
    } else {
        Ok(NpmDist { urls, checksum })
    }
}

/// List available versions of an npm package (sorted ascending), trying mirrors.
pub async fn list_versions(ctx: &Ctx, sources: &[Source], package: &str) -> Result<Vec<String>> {
    Ok(packument(ctx, sources, package).await?.versions)
}

pub async fn packument(ctx: &Ctx, sources: &[Source], package: &str) -> Result<NpmVersions> {
    #[derive(Deserialize)]
    struct Packument {
        #[serde(default)]
        versions: std::collections::BTreeMap<String, serde_json::Value>,
        #[serde(default, rename = "dist-tags")]
        dist_tags: std::collections::BTreeMap<String, String>,
    }
    let mut last_err: Option<Error> = None;
    for source in sources {
        let url = package_url(&source.download_url, package, None);
        match http::get_cached_source_json::<Packument>(ctx, source, &url).await {
            Ok(p) => {
                let mut versions: Vec<String> = p.versions.into_keys().collect();
                versions.sort_by(|a, b| crate::backend::python::cmp_versions(a, b));
                return Ok(NpmVersions {
                    versions,
                    dist_tags: p.dist_tags,
                });
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::other(format!("cannot list {package}"))))
}

pub async fn resolve_package_version(
    ctx: &Ctx,
    sources: &[Source],
    package: &str,
    backend: &str,
    request: &ToolRequest,
) -> Result<ToolVersion> {
    if let VersionSpec::Exact(version) = &request.spec {
        let prerelease = semver::Version::parse(version)
            .map(|version| !version.pre.is_empty())
            .unwrap_or(false);
        if prerelease
            && matches!(
                ctx.config.settings.prerelease,
                crate::config::PrereleasePolicy::Never
            )
        {
            return Err(Error::VersionResolve {
                tool: backend.into(),
                spec: version.clone(),
                hint: Some("pre-release versions are disabled".into()),
            });
        }
        let mut resolved = ToolVersion::new(backend, version);
        resolved.options = request.options.clone();
        return Ok(resolved);
    }
    let channel = match &request.spec {
        VersionSpec::Prefix(channel)
            if matches!(channel.as_str(), "canary" | "nightly" | "beta") =>
        {
            Some(channel.as_str())
        }
        _ => None,
    };
    if channel.is_some()
        && matches!(
            ctx.config.settings.prerelease,
            crate::config::PrereleasePolicy::Never
        )
    {
        return Err(Error::VersionResolve {
            tool: backend.into(),
            spec: request.spec.to_string(),
            hint: Some("pre-release channels are disabled".into()),
        });
    }
    let packument = packument(ctx, sources, package).await?;
    let version = if let Some(channel) = channel {
        packument
            .dist_tags
            .get(channel)
            .cloned()
            .ok_or_else(|| Error::VersionResolve {
                tool: backend.into(),
                spec: channel.into(),
                hint: Some("npm dist-tag is not published".into()),
            })?
    } else {
        let versions = packument
            .versions
            .into_iter()
            .map(|version| VersionInfo {
                stable: semver::Version::parse(&version)
                    .map(|version| version.pre.is_empty())
                    .unwrap_or(false),
                version,
                lts: None,
            })
            .collect::<Vec<_>>();
        crate::version::select_version_with_prerelease(
            &request.spec,
            &versions,
            ctx.config.settings.prerelease,
        )
        .ok_or_else(|| Error::VersionResolve {
            tool: backend.into(),
            spec: request.spec.to_string(),
            hint: Some("no version matched prerelease policy".into()),
        })?
        .version
        .clone()
    };
    let mut resolved = ToolVersion::new(backend, version);
    resolved.options = request.options.clone();
    Ok(resolved)
}

fn package_url(registry: &str, package: &str, version: Option<&str>) -> String {
    let package_url = http::join_url(registry, package);
    match version {
        Some(version) => http::join_url(&package_url, version),
        None => package_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn test_ctx(root: &std::path::Path) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        Ctx {
            dirs: dirs.clone(),
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store)),
            show_progress: false,
        }
    }

    #[test]
    fn builds_scoped_registry_urls() {
        assert_eq!(
            package_url(
                "https://registry.example.test/",
                "@oven/bun-linux-x64",
                Some("1.2.3")
            ),
            "https://registry.example.test/@oven/bun-linux-x64/1.2.3"
        );
        assert_eq!(
            package_url("https://registry.example.test", "bun", None),
            "https://registry.example.test/bun"
        );
    }

    #[tokio::test]
    async fn selected_sources_drive_metadata_and_download_failover() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let body = match path {
                    "/primary/tool" => r#"{"versions":{"1.0.0":{},"1.1.0":{}}}"#,
                    "/primary/tool/1.1.0" => {
                        r#"{"dist":{"tarball":"https://primary.invalid/tool.tgz","integrity":"sha512-AQID"}}"#
                    }
                    "/fallback/tool/1.1.0" => {
                        r#"{"dist":{"tarball":"https://fallback.invalid/tool.tgz","integrity":"sha512-AQID"}}"#
                    }
                    other => panic!("unexpected request path: {other}"),
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
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let ctx = Ctx {
            dirs: dirs.clone(),
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store)),
            show_progress: false,
        };
        let sources = vec![
            Source::official("primary", &format!("http://{address}/primary")),
            Source::mirror("fallback", &format!("http://{address}/fallback"), 10),
        ];

        let versions = list_versions(&ctx, &sources, "tool").await.unwrap();
        assert_eq!(versions, vec!["1.0.0", "1.1.0"]);
        let dist = resolve_dist(&ctx, &sources, "tool", "1.1.0").await.unwrap();
        assert_eq!(
            dist.urls,
            vec![
                "https://primary.invalid/tool.tgz",
                "https://fallback.invalid/tool.tgz"
            ]
        );
        assert!(dist.checksum.is_some());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn npm_metadata_applies_the_selected_sources_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.contains("x-registry-key: source-secret"));
            let body = r#"{"versions":{"1.0.0":{}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path());
        let mut source = Source::mirror("private", &format!("http://{address}/"), 1);
        source.forward_credentials = false;
        source.headers = vec![("X-Registry-Key".into(), "source-secret".into())];

        let versions = list_versions(&ctx, &[source], "tool").await.unwrap();
        assert_eq!(versions, vec!["1.0.0"]);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_integrity_disagreement_between_registries() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let integrity = if index == 0 {
                    "sha512-AQID"
                } else {
                    "sha512-BAUG"
                };
                let body = format!(
                    r#"{{"dist":{{"tarball":"https://example.invalid/tool.tgz","integrity":"{integrity}"}}}}"#
                );
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
        let ctx = test_ctx(temp.path());
        let sources = vec![
            Source::official("first", &format!("http://{address}/first")),
            Source::mirror("second", &format!("http://{address}/second"), 10),
        ];
        let error = resolve_dist(&ctx, &sources, "tool", "1.0.0")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("integrity mismatch"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn dist_tags_and_prerelease_policy_resolve_exact_versions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let body = r#"{"versions":{"1.0.0":{},"1.1.0-canary.1":{}},"dist-tags":{"latest":"1.0.0","canary":"1.1.0-canary.1"}}"#;
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
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(temp.path().join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(temp.path().join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(temp.path().join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        let mut ctx = Ctx {
            dirs: dirs.clone(),
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings: Default::default(),
                sources: Default::default(),
                tools: Default::default(),
                tool_configs: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store)),
            show_progress: false,
        };
        let sources = vec![Source::official("fixture", &format!("http://{address}"))];

        let latest = resolve_package_version(
            &ctx,
            &sources,
            "bun",
            "bun",
            &ToolRequest::parse("bun@latest").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(latest.version, "1.0.0");
        let canary = resolve_package_version(
            &ctx,
            &sources,
            "bun",
            "bun",
            &ToolRequest::parse("bun@canary").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(canary.version, "1.1.0-canary.1");
        ctx.config.settings.prerelease = crate::config::PrereleasePolicy::Allow;
        let allowed = resolve_package_version(
            &ctx,
            &sources,
            "bun",
            "bun",
            &ToolRequest::parse("bun@latest").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(allowed.version, "1.1.0-canary.1");
        ctx.config.settings.prerelease = crate::config::PrereleasePolicy::Never;
        assert!(resolve_package_version(
            &ctx,
            &sources,
            "bun",
            "bun",
            &ToolRequest::parse("bun@canary").unwrap(),
        )
        .await
        .is_err());
        assert!(resolve_package_version(
            &ctx,
            &sources,
            "bun",
            "bun",
            &ToolRequest::parse("bun@1.1.0-canary.1").unwrap(),
        )
        .await
        .is_err());
        server.join().unwrap();
    }
}
