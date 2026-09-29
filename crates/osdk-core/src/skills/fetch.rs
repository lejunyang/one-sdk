//! Fetching a skill's source from GitHub.
//!
//! P0-4b: resolve a (possibly floating) ref to an immutable commit, download the
//! repository tarball through the same source candidates and fail-closed
//! machinery the rest of osdk uses, and extract it into a scratch directory the
//! caller then reads with [`super::install::read_skill_dir`].
//!
//! Nothing here writes to an agent directory or the lock; it only produces an
//! on-disk source tree plus the resolved commit, so the CLI can treat a GitHub
//! source and a local path through one downstream path.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::pipeline::extract::{self, ArchiveKind};
use crate::source::Source;
use crate::{http, pipeline};

/// The result of fetching a GitHub source: the extracted repository root (top
/// directory already stripped) and the immutable commit it resolved to.
pub struct FetchedSource {
    /// Directory holding the repository contents (the tarball's top-level
    /// `<repo>-<commit>/` wrapper is already removed).
    pub root: PathBuf,
    /// The 40-hex commit the ref resolved to, pinned into the lock.
    pub commit: String,
}

#[derive(Deserialize)]
struct CommitRef {
    sha: String,
}

/// Built-in GitHub sources: the official host plus the CN proxy, matching the
/// `github:` backend's own defaults so a skill download benefits from the same
/// mirror failover.
fn github_sources() -> Vec<Source> {
    vec![
        Source::official("github", "https://github.com/").with_index("https://api.github.com/"),
        Source::mirror("ghproxy", "https://gh-proxy.com/https://github.com/", 10)
            .with_index("https://gh-proxy.com/https://api.github.com/"),
    ]
}

/// Resolve `owner/repo@ref` to an immutable commit sha.
///
/// `reference` accepts what the design doc's lock vocabulary implies: a bare
/// branch/tag/sha, or an explicit `branch:`/`tag:`/`rev:` selector. GitHub's
/// `commits/{ref}` endpoint resolves all of them to a sha, so a floating ref is
/// still pinned exactly once at install time.
pub async fn resolve_commit(
    ctx: &Ctx,
    owner: &str,
    repo: &str,
    reference: Option<&str>,
) -> Result<String> {
    let git_ref = normalize_ref(reference);
    let api = format!("https://api.github.com/repos/{owner}/{repo}/commits/{git_ref}");
    let sources = github_sources();
    let urls = http::github_url_candidates(&sources, &api);
    let commit: CommitRef = http::get_cached_github_json_from_urls(ctx, &api, &urls).await?;
    if commit.sha.len() < 7 || !commit.sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::other(format!(
            "GitHub returned an unexpected commit id `{}` for {owner}/{repo}@{git_ref}",
            commit.sha
        )));
    }
    Ok(commit.sha)
}

/// Download and extract `owner/repo` at an exact commit into a scratch tree.
///
/// Returns the extracted repository root with the tarball's single top-level
/// directory stripped. The archive is capped and extraction is bounded by the
/// existing pipeline extractor, so a hostile or oversized repo fails closed
/// rather than filling the disk.
pub async fn fetch_at_commit(ctx: &Ctx, owner: &str, repo: &str, commit: &str) -> Result<PathBuf> {
    // codeload serves the gzipped tarball; route it through the same source
    // candidates so a configured proxy is used when the direct host is slow.
    let canonical = format!("https://github.com/{owner}/{repo}/tar.gz/{commit}");
    let sources = github_sources();
    let candidates = http::github_url_candidates(&sources, &canonical);

    let scratch = ctx.dirs.tmp().join("skills-fetch").join(format!(
        "{owner}-{repo}-{}",
        &commit[..commit.len().min(12)]
    ));
    if scratch.exists() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    std::fs::create_dir_all(&scratch).map_err(|e| Error::io(&scratch, e))?;
    // codeload uses `github.com/<o>/<r>/tar.gz/...`, but the canonical download
    // host is github.com while the tarball is actually served by codeload; the
    // proxy candidates already rewrite the github.com host, and the official
    // candidate must target codeload where the tarball truly lives.
    fetch_archive_candidates(
        ctx,
        &candidates
            .iter()
            .map(|url| to_codeload(url))
            .collect::<Vec<_>>(),
        &scratch,
        &format!("{owner}/{repo}"),
    )
    .await
}

async fn fetch_archive_candidates(
    ctx: &Ctx,
    candidates: &[String],
    scratch: &Path,
    label: &str,
) -> Result<PathBuf> {
    let archive = scratch.join("source.tar.gz");
    let extracted = scratch.join("tree");
    let mut last_err: Option<Error> = None;
    for url in candidates {
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_dir_all(&extracted);
        let result = async {
            pipeline::download::download(&ctx.client, url, &archive, label, ctx.show_progress)
                .await?;
            extract::extract(&archive, &extracted, ArchiveKind::TarGz, true)
        }
        .await;
        match result {
            Ok(()) => {
                let _ = std::fs::remove_file(&archive);
                return Ok(extracted);
            }
            Err(error) => {
                tracing::warn!(
                    url,
                    "{}",
                    crate::i18n::trf("log.download_failover", &[("err", &error.to_string())])
                );
                last_err = Some(error);
            }
        }
    }
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&extracted);
    Err(last_err.unwrap_or_else(|| Error::other(format!("could not download {label}"))))
}

/// Resolve a source directory inside the extracted repo, applying an optional
/// subdir. Fails loudly when the subdir escapes the tree or does not exist.
pub fn subtree(root: &Path, subdir: Option<&str>) -> Result<PathBuf> {
    let Some(subdir) = subdir else {
        return Ok(root.to_path_buf());
    };
    // Reuse the skill path guard so `../` cannot climb out of the repo.
    let safe = super::safe_relative_path(subdir)?;
    let mut path = root.to_path_buf();
    for segment in safe.split('/') {
        path.push(segment);
    }
    if !path.is_dir() {
        return Err(Error::config(format!(
            "subdirectory `{subdir}` not found in the repository"
        )));
    }
    Ok(path)
}

/// Map a `github.com/<o>/<r>/tar.gz/<ref>` download URL onto the codeload host
/// that actually serves it, leaving an already-proxied URL untouched.
fn to_codeload(url: &str) -> String {
    match url.strip_prefix("https://github.com/") {
        Some(rest) => format!("https://codeload.github.com/{rest}"),
        None => url.to_string(),
    }
}

/// Normalize a version selector into a ref GitHub's `commits/{ref}` accepts.
///
/// `rev:`/`tag:`/`branch:` prefixes are osdk's own selector vocabulary; GitHub
/// wants the bare ref, so strip a known prefix and default to `HEAD`.
fn normalize_ref(reference: Option<&str>) -> String {
    match reference {
        None => "HEAD".to_string(),
        Some(value) => {
            let value = value.trim();
            for prefix in ["rev:", "tag:", "branch:"] {
                if let Some(rest) = value.strip_prefix(prefix) {
                    return rest.to_string();
                }
            }
            if value.is_empty() {
                "HEAD".to_string()
            } else {
                value.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn test_ctx(root: &Path) -> Ctx {
        let dirs = crate::dirs::Dirs::resolve_from(|key| match key {
            "OSDK_DATA_DIR" => Some(root.join("data").display().to_string()),
            "OSDK_CACHE_DIR" => Some(root.join("cache").display().to_string()),
            "OSDK_CONFIG_DIR" => Some(root.join("config").display().to_string()),
            _ => None,
        })
        .unwrap();
        dirs.ensure().unwrap();
        Ctx {
            cas: std::sync::Arc::new(crate::store::Cas::new(dirs.store.clone())),
            dirs,
            platform: crate::platform::Platform::current(),
            config: crate::config::Config::default(),
            client: reqwest::Client::new(),
            show_progress: false,
        }
    }

    fn tarball(contents: &[u8]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "repo-commit/SKILL.md", contents)
            .unwrap();
        archive.finish().unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }

    #[tokio::test]
    async fn invalid_archive_falls_through_to_the_next_skill_source() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let good = tarball(b"# skill");
        let server = std::thread::spawn(move || {
            for body in [b"not a tarball".to_vec(), good] {
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
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let temporary = tempfile::tempdir().unwrap();
        let scratch = temporary.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let urls = [
            format!("http://{address}/bad"),
            format!("http://{address}/good"),
        ];

        let root =
            fetch_archive_candidates(&test_ctx(temporary.path()), &urls, &scratch, "owner/repo")
                .await
                .unwrap();
        server.join().unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("SKILL.md")).unwrap(),
            "# skill"
        );
    }

    #[test]
    fn normalizes_selectors_to_bare_refs() {
        assert_eq!(normalize_ref(None), "HEAD");
        assert_eq!(normalize_ref(Some("")), "HEAD");
        assert_eq!(normalize_ref(Some("main")), "main");
        assert_eq!(normalize_ref(Some("branch:dev")), "dev");
        assert_eq!(normalize_ref(Some("tag:v1.2.3")), "v1.2.3");
        assert_eq!(normalize_ref(Some("rev:0123abcd")), "0123abcd");
    }

    #[test]
    fn codeload_rewrites_only_the_official_host() {
        assert_eq!(
            to_codeload("https://github.com/o/r/tar.gz/abc"),
            "https://codeload.github.com/o/r/tar.gz/abc"
        );
        // A proxied URL is left as-is: the proxy serves the tarball itself.
        let proxied = "https://gh-proxy.com/https://github.com/o/r/tar.gz/abc";
        assert_eq!(to_codeload(proxied), proxied);
    }

    #[test]
    fn subtree_rejects_escape_and_missing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("skills").join("a")).unwrap();
        assert_eq!(subtree(root, None).unwrap(), root);
        assert_eq!(
            subtree(root, Some("skills/a")).unwrap(),
            root.join("skills").join("a")
        );
        assert!(subtree(root, Some("../escape")).is_err());
        assert!(subtree(root, Some("skills/missing")).is_err());
    }
}
