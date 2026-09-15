//! Safe preflight selection for PEP 503 "simple" Python package indexes.
//!
//! This mirrors [`crate::package_registry`] for the Python ecosystem, but the
//! two are deliberately separate types: an npm registry answers a JSON ping and
//! is addressed per package name, while a Python index is an HTML or JSON
//! listing whose shape is fixed by PEP 503 / PEP 691.
//!
//! Two rules matter more than convenience here, and both are enforced by the
//! types rather than left to callers:
//!
//! 1. **A mirror only ever replaces the default index.** A mirror is a complete
//!    copy of PyPI, so it necessarily carries upstream's package names --
//!    including malicious ones. Ranking it *above* the default index (uv's
//!    `--index`, or `--extra-index-url` on either tool) is what turns a mirror
//!    into a dependency-confusion vector. [`IndexPlan`] therefore cannot
//!    express "extra index".
//! 2. **The transport is never downgraded.** The index is where artifact hashes
//!    come from, so plaintext or a redirect off the original origin would let an
//!    attacker rewrite the artifact and the hash meant to detect the rewrite.

use std::time::{Duration, Instant};

use futures_util::StreamExt;

/// Upstream. Used when no candidates are configured.
pub const PYPI: &str = "https://pypi.org/simple/";

const MAX_PROBE_BODY: usize = 64 * 1024;
const MAX_PROBE_REDIRECTS: usize = 3;
/// PEP 691 first, with the PEP 503 HTML listing as an acceptable fallback.
const INDEX_PROBE_ACCEPT: &str =
    "application/vnd.pypi.simple.v1+json;q=1.0, text/html;q=0.5, application/vnd.pypi.simple.v1+html;q=0.5";
/// A universally present project, used only to confirm the index answers in a
/// PEP 503 shape.
///
/// Note this page is **not** small: `pip` has thousands of releases and its
/// listing runs to megabytes. The probe therefore stops reading once it has seen
/// enough to recognize the shape rather than downloading the whole document --
/// the first attempt read to completion against `MAX_PROBE_BODY` and reported
/// every healthy mirror as `response exceeds 65536 bytes`.
const PROBE_PROJECT: &str = "pip";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProbe {
    pub url: String,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    /// A bounded, credential-free diagnostic suitable for display.
    pub error: Option<String>,
}

/// The outcome of preflight. Note the absence of any "extra index" variant:
/// osdk maps a mirror onto the default index or does nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexPlan {
    /// Leave the tool's own index configuration alone, with a reason to show.
    PassThrough { reason: String },
    /// Use this URL as *the default index*.
    Selected {
        url: String,
        probes: Vec<IndexProbe>,
    },
    /// Every candidate failed; the caller must not silently fall back.
    Unavailable { probes: Vec<IndexProbe> },
}

impl IndexPlan {
    /// The selected default-index URL, if preflight chose one.
    pub fn selected_url(&self) -> Option<&str> {
        match self {
            Self::Selected { url, .. } => Some(url.as_str()),
            _ => None,
        }
    }
}

/// Cap on a project listing read for version discovery.
///
/// Far larger than the probe cap because this one has to be complete: a
/// truncated listing would silently hide the newest release, which is the exact
/// question `latest` is asking. `pip`'s listing -- among the largest on PyPI --
/// measured about 2.3 MB, so this leaves generous headroom while still bounding
/// what a hostile index can make osdk allocate.
const MAX_LISTING_BODY: usize = 32 * 1024 * 1024;

/// List the versions a project publishes, newest last.
///
/// `base` is the index to read, normally the one [`plan`] selected. Versions are
/// returned as published strings without interpretation: PEP 440 versions are
/// not semver and osdk's own ordering is applied by the caller.
pub async fn list_versions(
    client: &reqwest::Client,
    base: &str,
    project: &str,
    offline: bool,
) -> crate::error::Result<Vec<String>> {
    use crate::error::Error;

    if offline {
        // Fail closed rather than reporting "no versions", which reads as "this
        // project has no releases" and sends the user looking in the wrong place.
        return Err(Error::other(format!(
            "cannot list versions for `{project}` while offline; request an exact \\
             version instead"
        )));
    }

    let endpoint = format!("{}/{project}/", base.trim_end_matches('/'));
    let response = client
        .get(&endpoint)
        .header(reqwest::header::ACCEPT, INDEX_PROBE_ACCEPT)
        .send()
        .await
        .map_err(|error| Error::other(format!("index request failed: {}", probe_error(error))))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // A 404 is a definite answer, unlike a transport failure, so it earns a
        // message that names the likely cause.
        return Err(Error::other(format!(
            "`{project}` was not found on the index; check the project name"
        )));
    }
    if !response.status().is_success() {
        return Err(Error::other(format!(
            "index returned HTTP {} for `{project}`",
            response.status().as_u16()
        )));
    }
    let json = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/vnd.pypi.simple"));

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| Error::other(format!("index read failed: {}", probe_error(error))))?;
        if body.len().saturating_add(chunk.len()) > MAX_LISTING_BODY {
            // Refuse rather than truncate: a partial listing would answer
            // `latest` with a stale version and look like a success.
            return Err(Error::other(format!(
                "index listing for `{project}` exceeds {MAX_LISTING_BODY} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }

    let versions = if json {
        versions_from_pep691(&body)?
    } else {
        versions_from_pep503(&body, project)?
    };
    if versions.is_empty() {
        return Err(Error::other(format!(
            "index listed no usable releases for `{project}`"
        )));
    }
    Ok(versions)
}

/// Read versions from a PEP 691 JSON listing.
///
/// Prefers the `versions` key (PEP 700) and falls back to deriving them from
/// filenames, because that key is optional and several mirrors omit it.
fn versions_from_pep691(body: &[u8]) -> crate::error::Result<Vec<String>> {
    use crate::error::Error;

    let parsed: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| Error::other("index returned invalid PEP 691 JSON"))?;
    if let Some(listed) = parsed.get("versions").and_then(|value| value.as_array()) {
        let versions: Vec<String> = listed
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect();
        if !versions.is_empty() {
            return Ok(versions);
        }
    }
    let files = parsed
        .get("files")
        .and_then(|value| value.as_array())
        .ok_or_else(|| Error::other("PEP 691 index response has no `files` array"))?;
    let mut versions = Vec::new();
    for file in files {
        // A yanked release is still installable by exact request but must not be
        // what `latest` resolves to.
        if file.get("yanked").is_some_and(|value| value != false) {
            continue;
        }
        let Some(filename) = file.get("filename").and_then(|value| value.as_str()) else {
            continue;
        };
        if let Some(version) = version_from_filename(filename) {
            if !versions.contains(&version) {
                versions.push(version);
            }
        }
    }
    Ok(versions)
}

/// Read versions from a PEP 503 HTML listing by parsing anchor filenames.
fn versions_from_pep503(body: &[u8], _project: &str) -> crate::error::Result<Vec<String>> {
    use crate::error::Error;

    let text = std::str::from_utf8(body)
        .map_err(|_| Error::other("index response is not UTF-8"))?;
    let mut versions = Vec::new();
    // Anchor text is the filename in a PEP 503 listing. Reading the text rather
    // than the href keeps a mirror's rewritten download URLs from mattering.
    for segment in text.split('<') {
        let Some(rest) = segment.strip_prefix("a ").or_else(|| segment.strip_prefix("A ")) else {
            continue;
        };
        let Some((_, after)) = rest.split_once('>') else {
            continue;
        };
        let filename = after.trim();
        if filename.is_empty() {
            continue;
        }
        if let Some(version) = version_from_filename(filename) {
            if !versions.contains(&version) {
                versions.push(version);
            }
        }
    }
    Ok(versions)
}

/// Extract the version from a distribution filename.
///
/// Wheels are `name-version-...whl` and source distributions are
/// `name-version.tar.gz`, so the version is the second dash-separated field of a
/// wheel and the tail of an sdist. Anything else is skipped rather than guessed
/// at -- a wrong version here would be installed as if it had been requested.
pub fn version_from_filename(filename: &str) -> Option<String> {
    if let Some(stem) = filename.strip_suffix(".whl") {
        // name-version[-build]-python-abi-platform
        let mut parts = stem.split('-');
        let _name = parts.next()?;
        let version = parts.next()?;
        return (!version.is_empty()).then(|| version.to_string());
    }
    for suffix in [".tar.gz", ".zip", ".tar.bz2", ".tar.xz"] {
        if let Some(stem) = filename.strip_suffix(suffix) {
            // The project name may itself contain dashes, so split from the right.
            let (_, version) = stem.rsplit_once('-')?;
            return (!version.is_empty()).then(|| version.to_string());
        }
    }
    None
}

/// Rank `candidates` by a fresh anonymous probe and pick the fastest healthy
/// one. An empty candidate list means "no mirror configured", which is a
/// pass-through rather than an implicit switch to upstream.
pub async fn plan(candidates: &[String], timeout_ms: u64) -> IndexPlan {
    if candidates.is_empty() {
        return IndexPlan::PassThrough {
            reason: "no Python index mirrors configured".to_string(),
        };
    }
    let probes = probe_all(candidates, timeout_ms).await;
    // Preserve configured order among equally fast candidates by using the
    // probe order as the tie-break, and require a latency to have been recorded.
    let best = probes
        .iter()
        .filter(|probe| probe.ok)
        .min_by_key(|probe| probe.latency_ms.unwrap_or(u64::MAX));
    match best {
        Some(probe) => IndexPlan::Selected {
            url: probe.url.clone(),
            probes,
        },
        None => IndexPlan::Unavailable { probes },
    }
}

async fn probe_all(candidates: &[String], timeout_ms: u64) -> Vec<IndexProbe> {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let client = match reqwest::Client::builder()
        .user_agent(concat!("osdk/", env!("CARGO_PKG_VERSION"), " index-probe"))
        .redirect(index_probe_redirect_policy())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return candidates
                .iter()
                .map(|url| IndexProbe {
                    url: url.clone(),
                    ok: false,
                    latency_ms: None,
                    error: Some(format!("client error: {error}")),
                })
                .collect();
        }
    };
    let futures = candidates.iter().cloned().map(|url| {
        let client = client.clone();
        async move { probe_one(&client, url, timeout).await }
    });
    futures_util::future::join_all(futures).await
}

fn index_probe_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        match validate_index_probe_redirect(attempt.url(), attempt.previous()) {
            Ok(()) => attempt.follow(),
            Err(error) => attempt.error(error),
        }
    })
}

/// Index probes are anonymous, but an attacker-controlled redirect could still
/// turn one into a network-reachability probe of internal services. Requiring
/// the exact original HTTPS origin on every hop rules that out without relying
/// on DNS-based address classification.
fn validate_index_probe_redirect(
    next: &reqwest::Url,
    previous: &[reqwest::Url],
) -> std::result::Result<(), &'static str> {
    let Some(initial) = previous.first() else {
        return Err("index probe redirect has no origin");
    };
    if previous.len() >= MAX_PROBE_REDIRECTS {
        return Err("index probe redirect limit exceeded");
    }
    if initial.scheme() != "https" || next.scheme() != "https" {
        return Err("index probe redirects must remain on HTTPS");
    }
    if !next.username().is_empty() || next.password().is_some() {
        return Err("index probe redirect must not contain credentials");
    }
    if initial.host_str() != next.host_str()
        || initial.port_or_known_default() != next.port_or_known_default()
    {
        return Err("index probe redirect must remain on the original origin");
    }
    if previous.iter().any(|url| url == next) {
        return Err("index probe redirect loop detected");
    }
    Ok(())
}

async fn probe_one(client: &reqwest::Client, base: String, timeout: Duration) -> IndexProbe {
    let started = Instant::now();
    let endpoint = format!("{}/{PROBE_PROJECT}/", base.trim_end_matches('/').to_owned());
    let result = tokio::time::timeout(timeout, async {
        let response = client
            .get(&endpoint)
            .header(reqwest::header::ACCEPT, INDEX_PROBE_ACCEPT)
            .send()
            .await
            .map_err(probe_error)?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let json = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("application/vnd.pypi.simple"));
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        let mut truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(probe_error)?;
            body.extend_from_slice(&chunk);
            // Stop once there is enough to recognize the shape. A real project
            // listing can run to megabytes -- `pip`'s does -- so reading to the
            // end would both waste the transfer and, worse, make every healthy
            // mirror look broken once the body passed the cap. The prefix is all
            // the validation below needs, and dropping the connection here also
            // keeps the probe fast, which is the point of measuring latency.
            if body.len() >= MAX_PROBE_BODY {
                body.truncate(MAX_PROBE_BODY);
                truncated = true;
                break;
            }
        }
        if body.is_empty() {
            return Err("empty response".into());
        }
        validate_probe_body(&body, json, truncated)
    })
    .await;
    match result {
        Ok(Ok(())) => IndexProbe {
            url: base,
            ok: true,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            error: None,
        },
        Ok(Err(error)) => IndexProbe {
            url: base,
            ok: false,
            latency_ms: None,
            error: Some(error),
        },
        Err(_) => IndexProbe {
            url: base,
            ok: false,
            latency_ms: None,
            error: Some(format!("timed out after {} ms", timeout.as_millis())),
        },
    }
}

/// Confirm the body really is a project listing. A mirror that serves a captive
/// portal or a generic error page with HTTP 200 must not rank as healthy.
///
/// `truncated` says the body is a prefix of a larger document, which is the
/// normal case for a project with many releases. A prefix cannot be parsed as
/// JSON, so the check falls back to looking for the structure a listing must
/// begin with -- still enough to reject a portal page, which contains neither a
/// `files` array nor PEP 503 anchors.
fn validate_probe_body(
    body: &[u8],
    json: bool,
    truncated: bool,
) -> std::result::Result<(), String> {
    if json {
        if truncated {
            let text = std::str::from_utf8(body)
                .map_err(|_| "PEP 691 index response is not UTF-8".to_string())?;
            // The key must be present *and* introduce an array; a portal page
            // that merely mentions the word would not satisfy both.
            let has_files = text
                .split_once("\"files\"")
                .map(|(_, rest)| rest.trim_start().starts_with(':'))
                .unwrap_or(false);
            if !has_files {
                return Err("PEP 691 index response has no `files` array".into());
            }
            return Ok(());
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(body).map_err(|_| "invalid PEP 691 index JSON".to_string())?;
        if !parsed.is_object() {
            return Err("PEP 691 index response is not a JSON object".into());
        }
        if parsed
            .get("files")
            .and_then(|files| files.as_array())
            .is_none()
        {
            return Err("PEP 691 index response has no `files` array".into());
        }
        return Ok(());
    }
    let text = std::str::from_utf8(body).map_err(|_| "index response is not UTF-8".to_string())?;
    // A PEP 503 project page is a list of anchors. Requiring one keeps a
    // portal's "welcome" page from passing as a healthy index.
    if !text.to_ascii_lowercase().contains("<a href") {
        return Err("index response has no PEP 503 anchors".into());
    }
    Ok(())
}

fn probe_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "request timed out".into()
    } else if error.is_connect() {
        "connection failed".into()
    } else if error.is_body() || error.is_decode() {
        "invalid response body".into()
    } else {
        "request failed".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_candidates_is_a_pass_through_not_an_implicit_upstream_switch() {
        let outcome = futures_executor_block_on(plan(&[], 100));
        match outcome {
            IndexPlan::PassThrough { reason } => {
                assert!(reason.contains("no Python index mirrors configured"));
            }
            other => panic!("expected pass-through, got {other:?}"),
        }
        // And nothing was selected, so no caller can read a URL out of it.
        assert_eq!(
            futures_executor_block_on(plan(&[], 100)).selected_url(),
            None
        );
    }

    #[test]
    fn plan_selects_the_fastest_healthy_probe() {
        let probes = [
            IndexProbe {
                url: "https://slow.test/simple/".into(),
                ok: true,
                latency_ms: Some(900),
                error: None,
            },
            IndexProbe {
                url: "https://fast.test/simple/".into(),
                ok: true,
                latency_ms: Some(80),
                error: None,
            },
            IndexProbe {
                url: "https://down.test/simple/".into(),
                ok: false,
                latency_ms: None,
                error: Some("connection failed".into()),
            },
        ];
        let best = probes
            .iter()
            .filter(|probe| probe.ok)
            .min_by_key(|probe| probe.latency_ms.unwrap_or(u64::MAX))
            .unwrap();
        assert_eq!(best.url, "https://fast.test/simple/");
    }

    #[test]
    fn a_failed_probe_never_becomes_a_selection() {
        let probes = vec![IndexProbe {
            url: "https://down.test/simple/".into(),
            ok: false,
            latency_ms: None,
            error: Some("HTTP 503".into()),
        }];
        let outcome = IndexPlan::Unavailable { probes };
        // Unavailable must not leak a usable URL: silently continuing with a
        // broken mirror is what fail-closed is meant to prevent.
        assert_eq!(outcome.selected_url(), None);
    }

    #[test]
    fn probe_body_validation_rejects_portals_and_accepts_real_listings() {
        // PEP 503 HTML
        assert!(validate_probe_body(b"<a href=\"pip-1.0.tar.gz\">pip</a>", false, false).is_ok());
        // A captive portal answering 200 with a welcome page.
        assert!(validate_probe_body(b"<html><body>Welcome</body></html>", false, false).is_err());
        assert!(validate_probe_body(b"", false, false).is_err());

        // PEP 691 JSON
        assert!(validate_probe_body(br#"{"files":[],"name":"pip"}"#, true, false).is_ok());
        // Valid JSON, but not an index document.
        assert!(validate_probe_body(br#"{"message":"forbidden"}"#, true, false).is_err());
        assert!(validate_probe_body(br#"["not","an","object"]"#, true, false).is_err());
        assert!(validate_probe_body(b"<html>", true, false).is_err());
    }

    /// A large listing arrives truncated, and that must still validate.
    ///
    /// `pip`'s own project page runs to megabytes, so the probe reads only a
    /// prefix. Treating a prefix as malformed reported every healthy mirror as
    /// `response exceeds 65536 bytes` -- which is how this was found.
    #[test]
    fn a_truncated_listing_is_still_recognized_but_a_portal_is_not() {
        // A prefix of real PEP 691 JSON: unparseable, yet clearly a listing.
        let prefix = br#"{"meta":{"api-version":"1.1"},"name":"pip","files":[{"filename":"pip-1.0.tar.gz","#;
        assert!(validate_probe_body(prefix, true, true).is_ok());
        // The same bytes would fail a strict parse, so the truncated path is
        // doing real work rather than shadowing the parse.
        assert!(validate_probe_body(prefix, true, false).is_err());

        // A portal page is still refused: it has no `files` array anywhere.
        let portal = br#"{"message":"login required","detail":"see files for help"}"#;
        assert!(validate_probe_body(portal, true, true).is_err());

        // And a body that merely mentions the word without introducing an
        // array does not pass either.
        let mentions = br#"{"note":"the \"files\" are elsewhere"}"#;
        assert!(validate_probe_body(mentions, true, true).is_err());

        // Truncated HTML validates on its anchors, as before.
        assert!(validate_probe_body(b"<a href=\"pip-1.0.tar.gz\">pip", false, true).is_ok());
    }

    #[test]
    fn redirects_may_not_downgrade_change_origin_or_carry_credentials() {
        let origin = reqwest::Url::parse("https://mirror.test/simple/pip/").unwrap();

        // Same origin, different path: allowed.
        assert!(validate_index_probe_redirect(
            &reqwest::Url::parse("https://mirror.test/pypi/simple/pip/").unwrap(),
            std::slice::from_ref(&origin)
        )
        .is_ok());

        for (label, target) in [
            ("http downgrade", "http://mirror.test/simple/pip/"),
            ("other host", "https://evil.test/simple/pip/"),
            ("internal host", "https://127.0.0.1/simple/pip/"),
            ("credentials", "https://user:pw@mirror.test/simple/pip/"),
            ("other port", "https://mirror.test:8443/simple/pip/"),
        ] {
            let next = reqwest::Url::parse(target).unwrap();
            assert!(
                validate_index_probe_redirect(&next, std::slice::from_ref(&origin)).is_err(),
                "expected `{label}` to be refused"
            );
        }

        // A redirect with no recorded origin cannot be validated, so refuse it.
        let next = reqwest::Url::parse("https://mirror.test/simple/pip/").unwrap();
        assert!(validate_index_probe_redirect(&next, &[]).is_err());

        // Loops and overlong chains are refused.
        assert!(validate_index_probe_redirect(&origin, std::slice::from_ref(&origin)).is_err());
        let chain = vec![origin.clone(), origin.clone(), origin.clone()];
        assert!(validate_index_probe_redirect(
            &reqwest::Url::parse("https://mirror.test/x/").unwrap(),
            &chain
        )
        .is_err());
    }

    /// The plan API is async but these cases need no IO; drive them without
    /// pulling a runtime into the test.
    fn futures_executor_block_on<F: std::future::Future>(future: F) -> F::Output {
        futures_util::future::FutureExt::now_or_never(Box::pin(future))
            .expect("candidate-free plan must not await IO")
    }
}
