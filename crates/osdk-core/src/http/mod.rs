//! Shared HTTP client factory and helpers for downloads and JSON index fetches.

use std::time::Duration;

use crate::backend::Ctx;
use crate::error::{Error, Result};

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
    let mut req = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = github_token() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let resp = req.send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// GitHub API variant of [`get_cached_json`], preserving GitHub headers and
/// token handling while adding stale/offline cache behavior.
pub async fn get_cached_github_json<T: serde::de::DeserializeOwned>(
    ctx: &Ctx,
    url: &str,
) -> Result<T> {
    get_cached_json_inner(ctx, url, true).await
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
        let mut request = ctx
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = github_token() {
            request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        request.send().await
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
    async fn offline_cache_miss_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let offline = test_ctx(temp.path(), true);
        let error = get_cached_text(&offline, "http://127.0.0.1:9/missing")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("offline metadata cache miss"));
    }
}
