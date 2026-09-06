//! Ambient mirror variables as ranked source candidates.
//!
//! Every managed toolchain has a native way to point it at a mirror through the
//! environment: `RUSTUP_DIST_SERVER` for rustup, `GOPROXY` for the go command,
//! `npm_config_registry` and friends for the npm family. Those variables are
//! genuinely useful — they are how a corporate mirror or a CI cache gets
//! configured — but obeying them unconditionally has two failure modes that both
//! showed up in practice:
//!
//! * A stale or wrong value wins silently. Nothing probes it, so a mirror that
//!   404s on the exact package being installed still beats a working built-in
//!   source, and the error surfaces as a download failure with no hint that an
//!   environment variable chose that host.
//! * A value that is not a usable URL at all is accepted and handed to the
//!   child process, which then fails with its own, less specific message.
//!
//! So an ambient value is treated as *a candidate*, not as a decision: it is
//! validated, turned into a [`Source`], and ranked alongside the built-in
//! mirrors by the same probe that ranks everything else. A value that does not
//! validate is dropped with a warning rather than silently ignored, because
//! "I set the variable and nothing happened" is precisely the situation this
//! module exists to avoid.
//!
//! [`SourceMode::Env`] exists for the opposite need: when the ambient value must
//! be obeyed even if it is slower or currently unreachable. There it is an error
//! for the variable to be missing or invalid, since silently falling back would
//! defeat the point of asking for it explicitly.

use crate::error::{Error, Result};
use crate::source::{Source, SourceKind};

/// How an ambient mirror variable participates in source selection.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    /// Validate the ambient value and rank it with the built-in sources.
    #[default]
    Auto,
    /// Use the ambient value alone; missing or invalid is an error.
    Env,
}

impl SourceMode {
    pub fn parse(value: &str) -> Option<SourceMode> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(SourceMode::Auto),
            "env" => Some(SourceMode::Env),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SourceMode::Auto => "auto",
            SourceMode::Env => "env",
        }
    }
}

impl std::str::FromStr for SourceMode {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        SourceMode::parse(value).ok_or_else(|| {
            Error::config(format!(
                "invalid source mode `{value}` (expected `auto` or `env`)"
            ))
        })
    }
}

impl std::fmt::Display for SourceMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The default endpoint rule for an ambient mirror: a plain HTTPS origin, or
/// loopback HTTP so a local caching proxy still works. Credentials, queries and
/// fragments are refused because they would be silently forwarded to whichever
/// host the probe happens to pick, and because most toolchains expect a bare
/// base URL here.
pub fn validate_https_endpoint(value: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(value)
        .map_err(|_| Error::config(format!("`{value}` is not a valid URL")))?;
    let loopback_http = parsed.scheme() == "http"
        && parsed
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
    if (parsed.scheme() != "https" && !loopback_http) || parsed.host_str().is_none() {
        return Err(Error::config(
            "a mirror must be an https URL with a host (http is allowed only for loopback)",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::config("a mirror URL must not embed credentials"));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::config(
            "a mirror URL must not carry a query string or fragment",
        ));
    }
    Ok(())
}

/// The source id given to a candidate derived from the environment. It is
/// deliberately distinct from every built-in id so `osdk source list` and the
/// probe cache can tell them apart, and so a pin can never accidentally name it.
pub const ENV_SOURCE_ID: &str = "env";

/// A mirror variable to consider, most specific first. Several toolchains accept
/// more than one spelling and the first non-empty one wins, matching how the
/// tool itself resolves them.
pub struct EnvMirror<'a> {
    /// Variables holding the download endpoint, most specific first.
    pub download: &'a [&'a str],
    /// Variables holding a separate index/update endpoint, if the tool has one.
    pub index: &'a [&'a str],
}

/// What reading the ambient variables produced.
pub enum EnvSource {
    /// No mirror variable was set, so there is nothing to add.
    Absent,
    /// A usable candidate derived from `variable`.
    Candidate { source: Source, variable: String },
    /// A value was present but cannot be used; `reason` explains why.
    Rejected { variable: String, reason: String },
}

fn first_non_empty<F>(names: &[&str], getenv: F) -> Option<(String, String)>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    names.iter().copied().find_map(|name| {
        let value = getenv(name)?;
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| (name.to_string(), trimmed.to_string()))
    })
}

/// Read the ambient mirror variables and turn them into a ranked candidate.
///
/// `validate` is the tool's own endpoint rule, so a value that the tool itself
/// would reject never becomes a candidate. Validation happens before any probe:
/// a malformed URL is a configuration error, not a reachability problem, and
/// reporting it as "unreachable" would send the user looking in the wrong place.
pub fn read_env_source<F, V>(mirror: &EnvMirror<'_>, getenv: F, validate: V) -> EnvSource
where
    F: Fn(&str) -> Option<String> + Copy,
    V: Fn(&str) -> Result<()>,
{
    let Some((variable, download)) = first_non_empty(mirror.download, getenv) else {
        return EnvSource::Absent;
    };
    if let Err(error) = validate(&download) {
        return EnvSource::Rejected {
            variable,
            reason: error.to_string(),
        };
    }
    let index = first_non_empty(mirror.index, getenv).map(|(_, value)| value);
    if let Some(index) = &index {
        if let Err(error) = validate(index) {
            return EnvSource::Rejected {
                variable,
                reason: error.to_string(),
            };
        }
    }
    let mut source = Source {
        id: ENV_SOURCE_ID.to_string(),
        kind: SourceKind::Custom,
        index_url: index,
        download_url: download,
        headers: Vec::new(),
        // An ambient endpoint is not necessarily the official one, so it does
        // not inherit the official source's credential-forwarding permission.
        forward_credentials: false,
        // Ordered selection should still prefer a deliberately configured
        // source; the env candidate wins on measured speed, not on priority.
        priority: 1,
        enabled: true,
    };
    // Keep the canonical form the validator implies, so the probe cache
    // fingerprint is stable across equivalent spellings.
    source.download_url = source.download_url.trim_end_matches('/').to_string();
    EnvSource::Candidate { source, variable }
}

/// Fold the ambient candidate into a backend's source list.
///
/// In `Auto` the candidate is appended and the caller's normal ranking decides;
/// a rejected value is reported through `warn` and dropped. In `Env` the
/// candidate replaces the list outright, and its absence or rejection is an
/// error rather than a quiet fallback.
pub fn apply_env_source<W>(
    mode: SourceMode,
    tool: &str,
    sources: Vec<Source>,
    env: EnvSource,
    warn: W,
) -> Result<Vec<Source>>
where
    W: Fn(&str),
{
    match (mode, env) {
        (SourceMode::Auto, EnvSource::Absent) => Ok(sources),
        (SourceMode::Auto, EnvSource::Candidate { source, variable }) => {
            let mut sources = sources;
            // A built-in source with the same endpoint would make the probe pay
            // twice for one host and report a confusing duplicate.
            let canonical = source.download_url.trim_end_matches('/').to_string();
            if sources
                .iter()
                .any(|existing| existing.download_url.trim_end_matches('/') == canonical)
            {
                tracing::debug!(
                    tool,
                    variable,
                    "ambient mirror matches a built-in source; not adding a duplicate candidate"
                );
                return Ok(sources);
            }
            tracing::debug!(tool, variable, url = %source.download_url,
                "ranking the ambient mirror alongside the built-in sources");
            sources.push(source);
            Ok(sources)
        }
        (SourceMode::Auto, EnvSource::Rejected { variable, reason }) => {
            warn(&crate::i18n::trf(
                "warn.env_source_invalid",
                &[
                    ("variable", variable.as_str()),
                    ("tool", tool),
                    ("reason", reason.as_str()),
                ],
            ));
            Ok(sources)
        }
        (SourceMode::Env, EnvSource::Candidate { source, .. }) => Ok(vec![source]),
        (SourceMode::Env, EnvSource::Absent) => Err(Error::config(crate::i18n::trf(
            "err.env_source_missing",
            &[("tool", tool)],
        ))),
        (SourceMode::Env, EnvSource::Rejected { variable, reason }) => {
            Err(Error::config(crate::i18n::trf(
                "err.env_source_invalid",
                &[
                    ("variable", variable.as_str()),
                    ("reason", reason.as_str()),
                ],
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_MIRROR: EnvMirror<'static> = EnvMirror {
        download: &["RUSTUP_DIST_SERVER"],
        index: &["RUSTUP_UPDATE_ROOT"],
    };

    fn https_only(value: &str) -> Result<()> {
        let url = reqwest::Url::parse(value).map_err(|_| Error::config("invalid URL"))?;
        if url.scheme() != "https" || url.host_str().is_none() {
            return Err(Error::config("must be an https URL with a host"));
        }
        Ok(())
    }

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> + Copy {
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn absent_variable_leaves_the_source_list_untouched() {
        let env = read_env_source(&RUST_MIRROR, env_of(&[]), https_only);
        assert!(matches!(env, EnvSource::Absent));
        let sources = vec![Source::official("official", "https://static.rust-lang.org")];
        let out = apply_env_source(SourceMode::Auto, "rust", sources, env, |_| {}).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "official");
    }

    #[test]
    fn a_valid_variable_becomes_one_more_ranked_candidate() {
        let env = read_env_source(
            &RUST_MIRROR,
            env_of(&[
                ("RUSTUP_DIST_SERVER", "https://mirror.test/rustup"),
                ("RUSTUP_UPDATE_ROOT", "https://mirror.test/rustup/rustup"),
            ]),
            https_only,
        );
        let sources = vec![Source::official("official", "https://static.rust-lang.org")];
        let out = apply_env_source(SourceMode::Auto, "rust", sources, env, |_| {}).unwrap();

        // Appended, not substituted: the built-ins stay available as fallbacks
        // and the probe decides which one actually gets used.
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].id, ENV_SOURCE_ID);
        assert_eq!(out[1].download_url, "https://mirror.test/rustup");
        assert_eq!(
            out[1].index_url.as_deref(),
            Some("https://mirror.test/rustup/rustup")
        );
        // An ambient endpoint must not be trusted with credentials.
        assert!(!out[1].forward_credentials);
    }

    /// Silently dropping a bad value is what made the original bug so hard to
    /// diagnose, so the warning is part of the contract.
    #[test]
    fn an_invalid_variable_warns_and_is_dropped_under_auto() {
        let env = read_env_source(
            &RUST_MIRROR,
            env_of(&[("RUSTUP_DIST_SERVER", "not a url")]),
            https_only,
        );
        assert!(matches!(env, EnvSource::Rejected { .. }));
        let sources = vec![Source::official("official", "https://static.rust-lang.org")];
        let warnings = std::cell::RefCell::new(Vec::new());
        let out = apply_env_source(SourceMode::Auto, "rust", sources, env, |message| {
            warnings.borrow_mut().push(message.to_string())
        })
        .unwrap();

        assert_eq!(out.len(), 1, "the invalid candidate must not be ranked");
        let warnings = warnings.into_inner();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("RUSTUP_DIST_SERVER"), "{:?}", warnings);
    }

    #[test]
    fn a_duplicate_endpoint_is_not_probed_twice() {
        let env = read_env_source(
            &RUST_MIRROR,
            env_of(&[("RUSTUP_DIST_SERVER", "https://rsproxy.cn")]),
            https_only,
        );
        let sources = vec![
            Source::official("official", "https://static.rust-lang.org"),
            Source::mirror("rsproxy", "https://rsproxy.cn", 5),
        ];
        let out = apply_env_source(SourceMode::Auto, "rust", sources, env, |_| {}).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|source| source.id != ENV_SOURCE_ID));
    }

    #[test]
    fn env_mode_uses_the_ambient_value_alone() {
        let env = read_env_source(
            &RUST_MIRROR,
            env_of(&[("RUSTUP_DIST_SERVER", "https://mirror.test/rustup")]),
            https_only,
        );
        let sources = vec![Source::official("official", "https://static.rust-lang.org")];
        let out = apply_env_source(SourceMode::Env, "rust", sources, env, |_| {}).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, ENV_SOURCE_ID);
    }

    /// Asking to obey the environment and then quietly not obeying it would be
    /// worse than failing, so both of these are errors rather than fallbacks.
    #[test]
    fn env_mode_refuses_to_fall_back() {
        let sources = vec![Source::official("official", "https://static.rust-lang.org")];

        let missing = read_env_source(&RUST_MIRROR, env_of(&[]), https_only);
        let error = apply_env_source(SourceMode::Env, "rust", sources.clone(), missing, |_| {})
            .expect_err("missing variable must fail in env mode");
        assert!(error.to_string().contains("rust"), "{error}");

        let invalid = read_env_source(
            &RUST_MIRROR,
            env_of(&[("RUSTUP_DIST_SERVER", "http://insecure.test")]),
            https_only,
        );
        let error = apply_env_source(SourceMode::Env, "rust", sources, invalid, |_| {})
            .expect_err("invalid variable must fail in env mode");
        assert!(error.to_string().contains("RUSTUP_DIST_SERVER"), "{error}");
    }

    #[test]
    fn the_most_specific_variable_wins() {
        let mirror = EnvMirror {
            download: &["BUN_CONFIG_REGISTRY", "npm_config_registry"],
            index: &[],
        };
        let env = read_env_source(
            &mirror,
            env_of(&[
                ("npm_config_registry", "https://generic.test"),
                ("BUN_CONFIG_REGISTRY", "https://specific.test"),
            ]),
            https_only,
        );
        let EnvSource::Candidate { source, variable } = env else {
            panic!("expected a candidate");
        };
        assert_eq!(variable, "BUN_CONFIG_REGISTRY");
        assert_eq!(source.download_url, "https://specific.test");
    }

    #[test]
    fn whitespace_only_values_count_as_absent() {
        let env = read_env_source(
            &RUST_MIRROR,
            env_of(&[("RUSTUP_DIST_SERVER", "   ")]),
            https_only,
        );
        assert!(matches!(env, EnvSource::Absent));
    }

    #[test]
    fn mode_round_trips_through_its_string_form() {
        for mode in [SourceMode::Auto, SourceMode::Env] {
            assert_eq!(SourceMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(SourceMode::parse("AUTO"), Some(SourceMode::Auto));
        assert_eq!(SourceMode::parse(" env "), Some(SourceMode::Env));
        assert_eq!(SourceMode::parse("nonsense"), None);
    }
}
