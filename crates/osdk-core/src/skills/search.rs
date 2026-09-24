//! Searching GitHub for installable skills.
//!
//! This mirrors what `npx skills find` actually does at the source level: it
//! talks to GitHub's public API anonymously and only reaches for a token when
//! the anonymous rate limit bites. It deliberately does **not** depend on
//! skills.sh -- the registry there needs its own auth and adds a third-party
//! coupling osdk does not want on the discovery path.
//!
//! Endpoint choice: GitHub's *code* search (`/search/code`) is the natural way
//! to find `SKILL.md` files but requires authentication for every call, so it
//! cannot be the anonymous default. *Repository* search (`/search/repositories`)
//! is anonymous-friendly (subject only to a lower unauthenticated rate limit),
//! so `find` searches repositories and lets the user pass a hit straight to
//! `osdk skills add <owner/repo>`, which then discovers the actual `SKILL.md`
//! files via the existing tarball path.
//!
//! Install-gated like the rest of `skills`.

use serde::Deserialize;

use crate::backend::Ctx;
use crate::error::{Error, Result};
use crate::http;
use crate::source::Source;

/// One repository hit worth showing as an installable skill source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHit {
    /// `owner/repo`, ready to hand to `osdk skills add`.
    pub full_name: String,
    /// Repository description, empty when GitHub has none.
    pub description: String,
    /// Star count, for ordering the eye down the list.
    pub stars: u64,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    items: Vec<RepoItem>,
}

#[derive(Deserialize)]
struct RepoItem {
    #[serde(default)]
    full_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    stargazers_count: u64,
}

/// Same built-in transports as the fetch path: official API plus the CN proxy,
/// so a search benefits from the identical mirror failover an install does.
fn github_sources() -> Vec<Source> {
    vec![
        Source::official("github", "https://github.com/").with_index("https://api.github.com/"),
        Source::mirror("ghproxy", "https://gh-proxy.com/https://github.com/", 10)
            .with_index("https://gh-proxy.com/https://api.github.com/"),
    ]
}

/// Search GitHub for repositories that look like installable skills.
///
/// `query` is the user's free-text terms (already joined). `owner` optionally
/// restricts to one org/user. `limit` caps the rows returned (clamped 1..=50).
///
/// The request goes out anonymously; `get_cached_github_json_from_urls` adds a
/// `GITHUB_TOKEN`/`GH_TOKEN` only when one is set and the transport is the
/// official API host, and it is what surfaces a clean rate-limit error (with the
/// hint to set `OSDK_GITHUB_TOKEN`) when the anonymous quota is exhausted.
pub async fn search(
    ctx: &Ctx,
    query: &str,
    owner: Option<&str>,
    limit: u8,
) -> Result<Vec<SkillHit>> {
    let limit = limit.clamp(1, 50);
    let q = build_query(query, owner)?;
    let encoded = urlencode(&q);
    let api = format!(
        "https://api.github.com/search/repositories?q={encoded}&sort=stars&order=desc&per_page={limit}"
    );
    let sources = github_sources();
    let urls = http::github_url_candidates(&sources, &api);
    let response: SearchResponse = http::get_cached_github_json_from_urls(ctx, &api, &urls).await?;

    Ok(response
        .items
        .into_iter()
        .filter(|item| !item.full_name.is_empty())
        .map(|item| SkillHit {
            full_name: item.full_name,
            description: item.description.unwrap_or_default(),
            stars: item.stargazers_count,
        })
        .collect())
}

/// Build the GitHub repository-search query string.
///
/// We always add `SKILL.md in:readme,description` heuristics? No -- repository
/// search cannot see file paths, so the closest anonymous signal is the topic
/// and the words "skill"/"skills". We bias toward that without excluding
/// legitimate repos: the user's terms are required, and when they gave nothing
/// we require an `--owner` so the query is still bounded.
fn build_query(query: &str, owner: Option<&str>) -> Result<String> {
    let query = query.trim();
    let mut parts: Vec<String> = Vec::new();
    if !query.is_empty() {
        parts.push(query.to_string());
    }
    if let Some(owner) = owner {
        let owner = owner.trim();
        if owner.is_empty() {
            return Err(Error::config("`--owner` was given but empty"));
        }
        // GitHub accepts both user: and org: via the generic `user:` qualifier
        // for repository search.
        parts.push(format!("user:{owner}"));
    }
    if parts.is_empty() {
        return Err(Error::config(
            "give search keywords, or restrict with `--owner <OWNER>`",
        ));
    }
    // Bias toward skill repositories without hard-excluding: the topic qualifier
    // is additive signal, and the `skill` keyword nudges ranking. We keep this a
    // soft nudge (a trailing keyword) rather than a required `topic:` so a repo
    // that simply holds a SKILL.md but never set the topic still surfaces.
    parts.push("skill".to_string());
    Ok(parts.join(" "))
}

/// Minimal percent-encoding for a search query value. GitHub's search endpoint
/// wants the `q` value URL-encoded; we avoid pulling a URL crate for one field
/// and encode the bytes that matter in a query component.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_requires_terms_or_owner() {
        assert!(build_query("", None).is_err());
        assert!(build_query("  ", None).is_err());
    }

    #[test]
    fn query_owner_only_is_bounded() {
        // Owner alone is allowed (browse an owner's skills); the skill nudge is
        // still appended.
        let q = build_query("", Some("vercel-labs")).unwrap();
        assert_eq!(q, "user:vercel-labs skill");
    }

    #[test]
    fn query_combines_terms_and_owner() {
        let q = build_query("typescript", Some("vercel")).unwrap();
        assert_eq!(q, "typescript user:vercel skill");
    }

    #[test]
    fn query_rejects_empty_owner_flag() {
        assert!(build_query("react", Some("   ")).is_err());
    }

    #[test]
    fn urlencode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(
            urlencode("typescript user:vercel"),
            "typescript%20user%3Avercel"
        );
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(urlencode("c++"), "c%2B%2B");
    }
}
