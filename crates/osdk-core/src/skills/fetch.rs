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
    let archive = scratch.join("source.tar.gz");

    // codeload uses `github.com/<o>/<r>/tar.gz/...`, but the canonical download
    // host is github.com while the tarball is actually served by codeload; the
    // proxy candidates already rewrite the github.com host, and the official
    // candidate must target codeload where the tarball truly lives.
    let mut last_err: Option<Error> = None;
    let mut downloaded = false;
    for url in candidates.iter().map(|u| to_codeload(u)) {
        match pipeline::download::download(
            &ctx.client,
            &url,
            &archive,
            &format!("{owner}/{repo}"),
            ctx.show_progress,
        )
        .await
        {
            Ok(()) => {
                downloaded = true;
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    if !downloaded {
        return Err(last_err.unwrap_or_else(|| {
            Error::other(format!("could not download {owner}/{repo}@{commit}"))
        }));
    }

    let extracted = scratch.join("tree");
    extract::extract(&archive, &extracted, ArchiveKind::TarGz, true)?;
    let _ = std::fs::remove_file(&archive);
    Ok(extracted)
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
