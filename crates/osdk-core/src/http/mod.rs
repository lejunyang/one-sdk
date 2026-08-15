//! Shared HTTP client factory and helpers for downloads and JSON index fetches.

use std::time::Duration;

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
}
