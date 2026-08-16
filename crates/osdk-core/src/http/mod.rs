//! Shared HTTP client factory and helpers for downloads and JSON index fetches.

use std::time::Duration;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::source::Source;

/// Build the shared reqwest client (rustls, gzip, redirects, sane timeouts).
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("osdk/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(Error::from)
}

/// Fetch a URL and deserialize the JSON body.
pub async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Fetch a URL and return the body as text.
pub async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.text().await?)
}

/// Fetch JSON with a persistent URL-keyed cache. Online requests refresh the
/// cache; failures fall back to stale data. Offline mode never makes a request.
pub async fn get_cached_json<T: serde::de::DeserializeOwned>(ctx: &Ctx, url: &str) -> Result<T> {
    get_cached_json_inner(ctx, url, false).await
}

/// Fetch text with the same stale-cache behavior as [`get_cached_json`].
pub async fn get_cached_text(ctx: &Ctx, url: &str) -> Result<String> {
    let cache_file = metadata_cache_path(ctx, url);
    let (bytes, fresh) = get_cached_bytes(ctx, url, false).await?;
    match String::from_utf8(bytes) {
        Ok(text) => {
            if fresh {
                write_metadata_cache(&cache_file, text.as_bytes());
            }
            Ok(text)
        }
        Err(error) if fresh => {
            let stale = std::fs::read(&cache_file)
                .map_err(|_| Error::other(format!("invalid UTF-8 from {url}: {error}")))?;
            String::from_utf8(stale).map_err(|stale_error| {
                Error::other(format!("invalid cached UTF-8 for {url}: {stale_error}"))
            })
        }
        Err(error) => Err(Error::other(format!("invalid UTF-8 from {url}: {error}"))),
    }
}

/// Fetch JSON from the GitHub API with the recommended headers, honoring a
/// `GITHUB_TOKEN`/`GH_TOKEN` env var to raise the rate limit when present.
/// GitHub returns 403 for API requests missing an `Accept`/`X-GitHub-Api-Version`
/// header under load, so we always send them.
pub async fn get_github_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T> {
    get_github_json_from_urls(client, &[url.to_string()]).await
}

/// Try multiple transports for one GitHub JSON resource. Authorization is sent
/// only to the official GitHub API host, never to a third-party proxy.
pub async fn get_github_json_from_urls<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    urls: &[String],
) -> Result<T> {
    let mut last_error = None;
    for url in urls {
        match fetch_github_bytes(client, url).await {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => return Ok(value),
                Err(error) => last_error = Some(Error::Json(error)),
            },
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::other("no GitHub API URL candidates")))
}

/// GitHub API variant of [`get_cached_json`], preserving GitHub headers and
/// token handling while adding stale/offline cache behavior.
pub async fn get_cached_github_json<T: serde::de::DeserializeOwned>(
    ctx: &Ctx,
    url: &str,
) -> Result<T> {
    get_cached_github_json_from_urls(ctx, url, &[url.to_string()]).await
}

/// Cached GitHub JSON with transport failover. `cache_identity` is the
/// canonical upstream URL, so direct and proxied transports share one cache.
pub async fn get_cached_github_json_from_urls<T: serde::de::DeserializeOwned>(
    ctx: &Ctx,
    cache_identity: &str,
    urls: &[String],
) -> Result<T> {
    let cache_file = metadata_cache_path(ctx, cache_identity);
    if ctx.config.settings.offline {
        let bytes = std::fs::read(&cache_file).map_err(|_| {
            Error::other(format!(
                "offline metadata cache miss for {cache_identity} (run once without --offline)"
            ))
        })?;
        return Ok(serde_json::from_slice(&bytes)?);
    }

    let mut last_error = None;
    for url in urls {
        match fetch_github_bytes(&ctx.client, url).await {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => {
                    write_metadata_cache(&cache_file, &bytes);
                    return Ok(value);
                }
                Err(error) => last_error = Some(Error::Json(error)),
            },
            Err(error) => last_error = Some(error),
        }
    }

    match std::fs::read(&cache_file) {
        Ok(bytes) => {
            tracing::warn!(
                path = %cache_file.display(),
                "using stale cached metadata after all GitHub transports failed"
            );
            Ok(serde_json::from_slice(&bytes)?)
        }
        Err(_) => Err(last_error.unwrap_or_else(|| Error::other("no GitHub API URL candidates"))),
    }
}

async fn get_cached_json_inner<T: serde::de::DeserializeOwned>(
    ctx: &Ctx,
    url: &str,
    github: bool,
) -> Result<T> {
    let cache_file = metadata_cache_path(ctx, url);
    let (bytes, fresh) = get_cached_bytes(ctx, url, github).await?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => {
            if fresh {
                write_metadata_cache(&cache_file, &bytes);
            }
            Ok(value)
        }
        Err(error) if fresh => {
            let stale = std::fs::read(&cache_file).map_err(|_| Error::Json(error))?;
            Ok(serde_json::from_slice(&stale)?)
        }
        Err(error) => Err(Error::Json(error)),
    }
}

async fn get_cached_bytes(ctx: &Ctx, url: &str, github: bool) -> Result<(Vec<u8>, bool)> {
    let cache_file = metadata_cache_path(ctx, url);
    if ctx.config.settings.offline {
        return std::fs::read(&cache_file)
            .map(|bytes| (bytes, false))
            .map_err(|_| {
                Error::other(format!(
                    "offline metadata cache miss for {url} (run once without --offline)"
                ))
            });
    }

    let result = if github {
        github_request(&ctx.client, url).send().await
    } else {
        ctx.client.get(url).send().await
    };

    match result {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.bytes().await {
                Ok(bytes) => Ok((bytes.to_vec(), true)),
                Err(error) => read_stale_or_error(&cache_file, Error::from(error)),
            },
            Err(error) => read_stale_or_error(&cache_file, Error::from(error)),
        },
        Err(error) => read_stale_or_error(&cache_file, Error::from(error)),
    }
}

async fn fetch_github_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = github_request(client, url)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.bytes().await?.to_vec())
}

pub(crate) fn github_request(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    let mut request = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if should_send_github_token(url) {
        if let Some(token) = github_token() {
            request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        }
    }
    request
}

fn should_send_github_token(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host.eq_ignore_ascii_case("api.github.com"))
}

/// Rewrite an official GitHub API, raw-content, or release URL through one
/// configured source. Sources whose bases embed the canonical GitHub URL (such
/// as `https://gh-proxy.com/https://github.com/`) also proxy raw content.
pub fn github_url_for_source(source: &Source, original: &str) -> String {
    const API_BASE: &str = "https://api.github.com/";
    const DOWNLOAD_BASE: &str = "https://github.com/";
    const RAW_BASE: &str = "https://raw.githubusercontent.com/";
    const GIST_BASE: &str = "https://gist.githubusercontent.com/";

    if let Some(path) = original.strip_prefix(API_BASE) {
        return source
            .index_url
            .as_deref()
            .map(|base| join_url(base, path))
            .unwrap_or_else(|| original.to_string());
    }
    if let Some(path) = original.strip_prefix(DOWNLOAD_BASE) {
        return join_url(&source.download_url, path);
    }
    if original.starts_with(RAW_BASE) || original.starts_with(GIST_BASE) {
        if let Some(prefix) = github_proxy_prefix(source) {
            return format!("{prefix}{original}");
        }
    }
    original.to_string()
}

/// Build unique candidate transports in source order for one GitHub resource.
pub fn github_url_candidates(sources: &[Source], original: &str) -> Vec<String> {
    let canonical = canonical_github_url(sources, original);
    let mut urls = Vec::new();
    for source in sources {
        let url = github_url_for_source(source, &canonical);
        if !urls.iter().any(|candidate| candidate == &url) {
            urls.push(url);
        }
    }
    if urls.is_empty() {
        urls.push(canonical);
    }
    urls
}

fn canonical_github_url(sources: &[Source], url: &str) -> String {
    for source in sources {
        if let Some(prefix) = github_proxy_prefix(source) {
            if let Some(original) = url.strip_prefix(prefix) {
                if original.starts_with("https://") {
                    return original.to_string();
                }
            }
        }
    }
    url.to_string()
}

fn github_proxy_prefix(source: &Source) -> Option<&str> {
    for value in [
        Some(source.download_url.as_str()),
        source.index_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        for canonical in ["https://github.com/", "https://api.github.com/"] {
            if let Some((prefix, _)) = value.split_once(canonical) {
                if !prefix.is_empty() {
                    return Some(prefix);
                }
            }
        }
    }
    None
}

fn metadata_cache_path(ctx: &Ctx, url: &str) -> std::path::PathBuf {
    let hash = blake3::hash(url.as_bytes()).to_hex().to_string();
    ctx.dirs.remote_cache().join("http").join(hash)
}

fn write_metadata_cache(path: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

fn read_stale_or_error(path: &std::path::Path, error: Error) -> Result<(Vec<u8>, bool)> {
    match std::fs::read(path) {
        Ok(bytes) => {
            tracing::warn!(path = %path.display(), "using stale cached metadata after request failure");
            Ok((bytes, false))
        }
        Err(_) => Err(error),
    }
}

/// Read a GitHub token from the usual env vars, if set and non-empty.
pub fn github_token() -> Option<String> {
    for key in ["OSDK_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Substitute `{version}`, `{os}`, `{arch}`, `{file}`, `{ext}` placeholders in a
/// URL template. Joins base + tail if the template ends with `/`.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Join a base URL (which may or may not end in `/`) with a path tail.
pub fn join_url(base: &str, tail: &str) -> String {
    let base = base.trim_end_matches('/');
    let tail = tail.trim_start_matches('/');
    format!("{base}/{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn test_ctx(root: &std::path::Path, offline: bool) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        let settings = crate::config::Settings {
            offline,
            ..Default::default()
        };
        Ctx {
            dirs: dirs.clone(),
            platform: crate::platform::Platform::current(),
            config: crate::config::Config {
                settings,
                sources: Default::default(),
                tools: Default::default(),
                aliases: Default::default(),
                project_config_path: None,
            },
            client: reqwest::Client::new(),
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            show_progress: false,
        }
    }

    #[test]
    fn template_render() {
        let t = "https://host/v{version}/node-v{version}-{os}-{arch}.{ext}";
        let got = render_template(
            t,
            &[
                ("version", "20.11.1"),
                ("os", "linux"),
                ("arch", "x64"),
                ("ext", "tar.gz"),
            ],
        );
        assert_eq!(got, "https://host/v20.11.1/node-v20.11.1-linux-x64.tar.gz");
    }

    #[test]
    fn url_join() {
        assert_eq!(
            join_url("https://h/dist/", "/index.json"),
            "https://h/dist/index.json"
        );
        assert_eq!(
            join_url("https://h/dist", "index.json"),
            "https://h/dist/index.json"
        );
    }

    #[test]
    fn github_source_rewrites_api_raw_and_release_urls() {
        let direct =
            Source::official("github", "https://github.com/").with_index("https://api.github.com/");
        let proxy = Source::mirror("ghproxy", "https://gh-proxy.com/https://github.com/", 10)
            .with_index("https://gh-proxy.com/https://api.github.com/");

        assert_eq!(
            github_url_for_source(
                &direct,
                "https://api.github.com/repos/cli/cli/releases?per_page=30"
            ),
            "https://api.github.com/repos/cli/cli/releases?per_page=30"
        );
        assert_eq!(
            github_url_for_source(
                &proxy,
                "https://api.github.com/repos/cli/cli/releases?per_page=30"
            ),
            "https://gh-proxy.com/https://api.github.com/repos/cli/cli/releases?per_page=30"
        );
        assert_eq!(
            github_url_for_source(
                &proxy,
                "https://github.com/cli/cli/releases/download/v1.0.0/gh.tar.gz"
            ),
            "https://gh-proxy.com/https://github.com/cli/cli/releases/download/v1.0.0/gh.tar.gz"
        );
        assert_eq!(
            github_url_for_source(
                &proxy,
                "https://raw.githubusercontent.com/cli/cli/main/README.md"
            ),
            "https://gh-proxy.com/https://raw.githubusercontent.com/cli/cli/main/README.md"
        );
        assert_eq!(
            github_url_for_source(
                &proxy,
                "https://gist.githubusercontent.com/user/id/raw/file"
            ),
            "https://gh-proxy.com/https://gist.githubusercontent.com/user/id/raw/file"
        );
    }

    #[test]
    fn github_candidates_follow_source_order_without_duplicates() {
        let proxy = Source::mirror("ghproxy", "https://gh-proxy.com/https://github.com/", 10)
            .with_index("https://gh-proxy.com/https://api.github.com/");
        let direct =
            Source::official("github", "https://github.com/").with_index("https://api.github.com/");
        let duplicate = Source::mirror("duplicate", "https://github.com/", 20)
            .with_index("https://api.github.com/");

        assert_eq!(
            github_url_candidates(
                &[proxy, direct, duplicate],
                "https://api.github.com/repos/cli/cli/releases"
            ),
            vec![
                "https://gh-proxy.com/https://api.github.com/repos/cli/cli/releases",
                "https://api.github.com/repos/cli/cli/releases",
            ]
        );
    }

    #[test]
    fn github_candidates_canonicalize_a_locked_proxy_url() {
        let proxy = Source::mirror("ghproxy", "https://gh-proxy.com/https://github.com/", 10)
            .with_index("https://gh-proxy.com/https://api.github.com/");
        let direct =
            Source::official("github", "https://github.com/").with_index("https://api.github.com/");

        assert_eq!(
            github_url_candidates(
                &[direct, proxy],
                "https://gh-proxy.com/https://github.com/cli/cli/releases/download/v1/gh.tar.gz"
            ),
            vec![
                "https://github.com/cli/cli/releases/download/v1/gh.tar.gz",
                "https://gh-proxy.com/https://github.com/cli/cli/releases/download/v1/gh.tar.gz",
            ]
        );
    }

    #[test]
    fn github_token_is_limited_to_official_api_host() {
        assert!(should_send_github_token(
            "https://api.github.com/repos/cli/cli/releases"
        ));
        assert!(!should_send_github_token(
            "https://gh-proxy.com/https://api.github.com/repos/cli/cli/releases"
        ));
        assert!(!should_send_github_token(
            "https://raw.githubusercontent.com/cli/cli/main/README.md"
        ));
    }

    #[tokio::test]
    async fn cached_json_is_available_offline_without_a_request() {
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
            let body = r#"{"versions":["1.2.3"]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let url = format!("http://{address}/metadata.json");
        let online = test_ctx(temp.path(), false);
        let value: serde_json::Value = get_cached_json(&online, &url).await.unwrap();
        assert_eq!(value["versions"][0], "1.2.3");
        server.join().unwrap();

        let offline = test_ctx(temp.path(), true);
        let value: serde_json::Value = get_cached_json(&offline, &url).await.unwrap();
        assert_eq!(value["versions"][0], "1.2.3");
    }

    #[tokio::test]
    async fn cached_github_json_fails_over_transports_and_reuses_one_cache() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for status in ["503 Service Unavailable", "200 OK"] {
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
                let body = if status == "200 OK" {
                    r#"{"versions":["2.0.0"]}"#
                } else {
                    r#"{"message":"retry elsewhere"}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let temp = tempfile::tempdir().unwrap();
        let identity = "https://api.github.com/repos/example/tool/releases";
        let urls = vec![
            format!("http://{address}/direct"),
            format!("http://{address}/proxy"),
        ];
        let online = test_ctx(temp.path(), false);
        let value: serde_json::Value = get_cached_github_json_from_urls(&online, identity, &urls)
            .await
            .unwrap();
        assert_eq!(value["versions"][0], "2.0.0");
        server.join().unwrap();

        let offline = test_ctx(temp.path(), true);
        let value: serde_json::Value = get_cached_github_json_from_urls(&offline, identity, &urls)
            .await
            .unwrap();
        assert_eq!(value["versions"][0], "2.0.0");
    }

    #[tokio::test]
    async fn offline_cache_miss_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let offline = test_ctx(temp.path(), true);
        let error = get_cached_text(&offline, "http://127.0.0.1:9/missing")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("offline metadata cache miss"));
    }
}
