//! Shared HTTP client factory and helpers for downloads and JSON index fetches.

use std::time::Duration;

use crate::backend::Ctx;
use crate::error::{Error, GithubRateLimitInfo, Result};
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
    let mut rate_limit_error = None;
    let token_configured =
        github_token().is_some() && urls.iter().any(|url| should_send_github_token(url));
    for url in urls {
        match fetch_github_bytes(client, url).await {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => return Ok(value),
                Err(error) => last_error = Some(Error::Json(error)),
            },
            Err(error) if matches!(error, Error::GithubRateLimited { .. }) => {
                remember_rate_limit(&mut rate_limit_error, error, token_configured);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(select_github_error(rate_limit_error, last_error))
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
    let mut rate_limit_error = None;
    let token_configured =
        github_token().is_some() && urls.iter().any(|url| should_send_github_token(url));
    for url in urls {
        match fetch_github_bytes(&ctx.client, url).await {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => {
                    write_metadata_cache(&cache_file, &bytes);
                    return Ok(value);
                }
                Err(error) => last_error = Some(Error::Json(error)),
            },
            Err(error) if matches!(error, Error::GithubRateLimited { .. }) => {
                remember_rate_limit(&mut rate_limit_error, error, token_configured);
            }
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
        Err(_) => Err(select_github_error(rate_limit_error, last_error)),
    }
}

fn remember_rate_limit(slot: &mut Option<Error>, error: Error, token_configured: bool) {
    let authenticated = matches!(
        error,
        Error::GithubRateLimited {
            authenticated: true,
            ..
        }
    );
    if (authenticated || !token_configured) && (authenticated || slot.is_none()) {
        *slot = Some(error);
    }
}

fn select_github_error(rate_limit_error: Option<Error>, last_error: Option<Error>) -> Error {
    match (rate_limit_error, last_error) {
        (Some(rate_limit), Some(last))
            if matches!(
                rate_limit,
                Error::GithubRateLimited {
                    authenticated: false,
                    ..
                }
            ) && last.status() == Some(403) =>
        {
            last
        }
        (Some(rate_limit), _) => rate_limit,
        (None, Some(last)) => last,
        (None, None) => Error::other("no GitHub API URL candidates"),
    }
}

/// Fetch public GitHub Web metadata through ordered transports, sharing one
/// canonical cache entry and retaining stale/offline behavior. Unlike API
/// requests this deliberately sends neither API headers nor authorization.
pub async fn get_cached_text_from_urls(
    ctx: &Ctx,
    cache_identity: &str,
    urls: &[String],
    validator: impl Fn(&str) -> bool,
) -> Result<String> {
    let cache_file = metadata_cache_path(ctx, cache_identity);
    if ctx.config.settings.offline {
        let bytes = std::fs::read(&cache_file).map_err(|_| {
            Error::other(format!(
                "offline metadata cache miss for {cache_identity} (run once without --offline)"
            ))
        })?;
        let text = String::from_utf8(bytes).map_err(|error| {
            Error::other(format!(
                "invalid cached UTF-8 for {cache_identity}: {error}"
            ))
        })?;
        return validator(&text)
            .then_some(text)
            .ok_or_else(|| invalid_metadata(cache_identity));
    }

    let mut last_error = None;
    for url in urls {
        match fetch_public_bytes(&ctx.client, url).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) if validator(&text) => {
                    write_metadata_cache(&cache_file, text.as_bytes());
                    return Ok(text);
                }
                Ok(_) => last_error = Some(invalid_metadata(url)),
                Err(error) => {
                    last_error = Some(Error::other(format!("invalid UTF-8 from {url}: {error}")))
                }
            },
            Err(error) => last_error = Some(error),
        }
    }

    match std::fs::read(&cache_file) {
        Ok(bytes) => {
            tracing::warn!(
                path = %cache_file.display(),
                "using stale cached metadata after all public GitHub transports failed"
            );
            let text = String::from_utf8(bytes).map_err(|error| {
                Error::other(format!(
                    "invalid cached UTF-8 for {cache_identity}: {error}"
                ))
            })?;
            validator(&text)
                .then_some(text)
                .ok_or_else(|| last_error.unwrap_or_else(|| invalid_metadata(cache_identity)))
        }
        Err(_) => {
            Err(last_error.unwrap_or_else(|| Error::other("no public GitHub URL candidates")))
        }
    }
}

fn invalid_metadata(url: &str) -> Error {
    Error::Network {
        kind: crate::error::NetworkErrorKind::InvalidMetadata,
        url: url.into(),
        status: None,
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
        Err(_) if fresh => {
            let stale = std::fs::read(&cache_file).map_err(|_| Error::Network {
                kind: crate::error::NetworkErrorKind::InvalidMetadata,
                url: url.into(),
                status: None,
            })?;
            Ok(serde_json::from_slice(&stale)?)
        }
        Err(_) => Err(Error::Network {
            kind: crate::error::NetworkErrorKind::InvalidMetadata,
            url: url.into(),
            status: None,
        }),
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
                Err(error) => read_stale_or_error(&cache_file, Error::network(url, error)),
            },
            Err(error) => read_stale_or_error(&cache_file, Error::network(url, error)),
        },
        Err(error) => read_stale_or_error(&cache_file, Error::network(url, error)),
    }
}

async fn fetch_github_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = github_request(client, url)
        .send()
        .await
        .map_err(|error| Error::network(url, error))?;
    if !response.status().is_success() {
        return Err(github_response_error(
            url,
            should_send_github_token(url) && github_token().is_some(),
            response,
        )
        .await);
    }
    Ok(response
        .bytes()
        .await
        .map_err(|error| Error::network(url, error))?
        .to_vec())
}

async fn fetch_public_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| Error::network(url, error))?
        .error_for_status()
        .map_err(|error| Error::network(url, error))?;
    Ok(response
        .bytes()
        .await
        .map_err(|error| Error::network(url, error))?
        .to_vec())
}

async fn github_response_error(
    url: &str,
    authenticated: bool,
    mut response: reqwest::Response,
) -> Error {
    const MAX_ERROR_BODY: usize = 64 * 1024;

    let status = response.status();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    while body.len() < MAX_ERROR_BODY {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = MAX_ERROR_BODY - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    let body_text = String::from_utf8_lossy(&body);
    let body_json = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let message = body_json
        .as_ref()
        .and_then(|value| value.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let documentation = body_json
        .as_ref()
        .and_then(|value| value.get("documentation_url"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let lower_body = body_text.to_ascii_lowercase();
    let retry_after = header_text(&headers, reqwest::header::RETRY_AFTER);
    let remaining_is_zero =
        header_text(&headers, "x-ratelimit-remaining").is_some_and(|value| value.trim() == "0");
    let github_api_response = should_send_github_token(url)
        || headers.contains_key("x-github-request-id")
        || headers.contains_key("x-ratelimit-resource")
        || (headers.contains_key("x-ratelimit-limit")
            && headers.contains_key("x-ratelimit-remaining")
            && headers.contains_key("x-ratelimit-reset"));
    let rate_limited = github_api_response
        && (status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || (status == reqwest::StatusCode::FORBIDDEN
                && (retry_after.is_some()
                    || remaining_is_zero
                    || lower_body.contains("api rate limit exceeded")
                    || lower_body.contains("secondary rate limit")
                    || lower_body.contains("abuse detection mechanism")
                    || documentation.to_ascii_lowercase().contains("rate-limit")
                    || documentation.to_ascii_lowercase().contains("rate_limits"))));

    if rate_limited {
        return Error::GithubRateLimited {
            url: url.into(),
            status: status.as_u16(),
            authenticated,
            info: GithubRateLimitInfo {
                message,
                reset: header_text(&headers, "x-ratelimit-reset"),
                retry_after,
            },
        };
    }

    response
        .error_for_status()
        .map(|_| unreachable!("non-success GitHub response became successful"))
        .unwrap_or_else(|error| Error::network(url, error))
}

fn header_text(
    headers: &reqwest::header::HeaderMap,
    name: impl reqwest::header::AsHeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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

pub(crate) fn metadata_cache_path(ctx: &Ctx, url: &str) -> std::path::PathBuf {
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

    #[tokio::test]
    async fn network_failure_matrix_has_stable_error_kinds_and_stale_fallback() {
        use crate::error::NetworkErrorKind;

        for (status, expected) in [
            ("403 Forbidden", NetworkErrorKind::Forbidden),
            ("429 Too Many Requests", NetworkErrorKind::RateLimited),
            ("503 Service Unavailable", NetworkErrorKind::Server),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            });
            let temp = tempfile::tempdir().unwrap();
            let ctx = test_ctx(temp.path(), false);
            let url = format!("http://{address}/metadata");
            let error = get_cached_json::<serde_json::Value>(&ctx, &url)
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                Error::Network { kind, .. } if kind == expected
            ));
            server.join().unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(temp.path(), false);
        let url = "http://127.0.0.1:9/unreachable";
        let error = get_cached_json::<serde_json::Value>(&ctx, url)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Network {
                kind: NetworkErrorKind::Connect,
                ..
            }
        ));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let body = "not-json";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let malformed_url = format!("http://{address}/metadata");
        let malformed = get_cached_json::<serde_json::Value>(&ctx, &malformed_url)
            .await
            .unwrap_err();
        assert!(matches!(
            malformed,
            Error::Network {
                kind: NetworkErrorKind::InvalidMetadata,
                ..
            }
        ));
        server.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let stale_url = format!("http://{address}/stale");
        let stale_path = metadata_cache_path(&ctx, &stale_url);
        write_metadata_cache(&stale_path, br#"{"cached":true}"#);
        let stale: serde_json::Value = get_cached_json(&ctx, &stale_url).await.unwrap();
        assert_eq!(stale["cached"], true);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn request_timeout_is_classified() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(250));
        });
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(temp.path(), false);
        ctx.client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(30))
            .build()
            .unwrap();
        let url = format!("http://{address}/slow");
        let error = get_cached_json::<serde_json::Value>(&ctx, &url)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Network {
                kind: crate::error::NetworkErrorKind::Timeout,
                ..
            }
        ));
        server.join().unwrap();
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
    async fn github_403_distinguishes_rate_limit_from_forbidden() {
        for (headers, body, rate_limited) in [
            (
                "X-RateLimit-Limit: 60\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1787446800\r\nRetry-After: 60\r\n",
                r#"{"message":"API rate limit exceeded for 203.0.113.10."}"#,
                true,
            ),
            (
                "X-RateLimit-Remaining: 4998\r\n",
                r#"{"message":"Resource not accessible by integration","documentation_url":"https://docs.github.com/rest/releases/releases"}"#,
                false,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                write!(
                    stream,
                    "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
            });
            let url = format!("http://{address}/repos/example/tool/releases");
            let error =
                get_github_json_from_urls::<serde_json::Value>(&reqwest::Client::new(), &[url])
                    .await
                    .unwrap_err();
            if rate_limited {
                match error {
                    Error::GithubRateLimited {
                        authenticated,
                        info,
                        ..
                    } => {
                        assert!(!authenticated);
                        assert_eq!(info.reset.as_deref(), Some("1787446800"));
                        assert_eq!(info.retry_after.as_deref(), Some("60"));
                        assert!(info.message.unwrap().contains("rate limit exceeded"));
                    }
                    other => panic!("expected rate-limit error, got {other}"),
                }
            } else {
                assert!(matches!(
                    error,
                    Error::Network {
                        kind: crate::error::NetworkErrorKind::Forbidden,
                        status: Some(403),
                        ..
                    }
                ));
            }
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn third_party_429_is_not_treated_as_anonymous_github_quota() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let body = r#"{"message":"proxy quota exhausted"}"#;
            write!(
                stream,
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            )
            .unwrap();
        });
        let url = format!("http://{address}/proxy");
        let error = get_github_json_from_urls::<serde_json::Value>(&reqwest::Client::new(), &[url])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Network {
                kind: crate::error::NetworkErrorKind::RateLimited,
                ..
            }
        ));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn github_rate_limit_survives_a_later_proxy_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for (status, headers, body) in [
                (
                    "403 Forbidden",
                    "X-RateLimit-Limit: 60\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1787446800\r\n",
                    r#"{"message":"API rate limit exceeded"}"#,
                ),
                (
                    "503 Service Unavailable",
                    "",
                    r#"{"message":"proxy unavailable"}"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
            }
        });
        let urls = vec![
            format!("http://{address}/official"),
            format!("http://{address}/proxy"),
        ];
        let error = get_github_json_from_urls::<serde_json::Value>(&reqwest::Client::new(), &urls)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::GithubRateLimited { .. }));
        assert!(error.to_string().contains("1787446800"));
        assert!(error.to_string().contains("OSDK_GITHUB_TOKEN"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn public_metadata_skips_invalid_success_and_caches_only_valid_text() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for body in ["<html>proxy error</html>", "<feed><entry/></feed>"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let identity = "https://github.com/example/tool/releases.atom";
        let urls = vec![
            format!("http://{address}/bad-proxy"),
            format!("http://{address}/valid-direct"),
        ];
        let online = test_ctx(temp.path(), false);
        let text =
            get_cached_text_from_urls(&online, identity, &urls, |text| text.contains("<feed>"))
                .await
                .unwrap();
        assert_eq!(text, "<feed><entry/></feed>");
        server.join().unwrap();

        let offline = test_ctx(temp.path(), true);
        let text =
            get_cached_text_from_urls(&offline, identity, &urls, |text| text.contains("<feed>"))
                .await
                .unwrap();
        assert_eq!(text, "<feed><entry/></feed>");
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
