//! Version request / resolution types shared across backends.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::{Error, Result};

pub mod resolver;

/// What the user asked for, before resolution against remote versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionSpec {
    /// Newest stable version.
    Latest,
    /// Latest LTS, optionally a named line (e.g. `lts/iron`).
    Lts(Option<String>),
    /// A prefix match, e.g. `20` matches `20.x.y`, `20.11` matches `20.11.z`.
    Prefix(String),
    /// An exact version, e.g. `20.11.1`.
    Exact(String),
    /// A semver requirement from structured project metadata.
    Range(String),
    /// Use whatever is already on PATH (no management).
    System,
}

impl VersionSpec {
    pub fn parse(s: &str) -> VersionSpec {
        let s = s.trim();
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "latest" | "current" | "stable" | "" => return VersionSpec::Latest,
            "system" => return VersionSpec::System,
            "lts" | "lts/*" | "lts-latest" => return VersionSpec::Lts(None),
            _ => {}
        }
        if let Some(rest) = lower.strip_prefix("lts/") {
            return VersionSpec::Lts(Some(rest.to_string()));
        }
        if let Some(rest) = lower.strip_prefix("lts-") {
            return VersionSpec::Lts(Some(rest.to_string()));
        }
        // A fully-specified semver (x.y.z, possibly with pre/build) is exact;
        // anything shorter is treated as a prefix.
        let core = s.strip_prefix('v').unwrap_or(s);
        if semver::Version::parse(core).is_ok() {
            VersionSpec::Exact(core.to_string())
        } else {
            VersionSpec::Prefix(core.to_string())
        }
    }

    pub fn parse_range(s: &str) -> Result<VersionSpec> {
        npm_range_requirements(s)?
            .first()
            .ok_or_else(|| Error::config("empty semver range"))?;
        Ok(VersionSpec::Range(s.trim().to_string()))
    }
}

fn npm_range_requirements(input: &str) -> Result<Vec<semver::VersionReq>> {
    input
        .split("||")
        .map(|alternative| {
            let normalized = normalize_npm_comparators(alternative)?;
            semver::VersionReq::parse(&normalized)
                .map_err(|error| Error::config(format!("invalid semver range `{input}`: {error}")))
        })
        .collect()
}

fn normalize_npm_comparators(input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::config("empty semver range alternative"));
    }
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.len() > 1 {
        Ok(tokens.join(", "))
    } else {
        Ok(input.to_string())
    }
}

impl fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionSpec::Latest => write!(f, "latest"),
            VersionSpec::Lts(None) => write!(f, "lts"),
            VersionSpec::Lts(Some(n)) => write!(f, "lts/{n}"),
            VersionSpec::Prefix(p) => write!(f, "{p}"),
            VersionSpec::Exact(v) => write!(f, "{v}"),
            VersionSpec::Range(requirement) => write!(f, "{requirement}"),
            VersionSpec::System => write!(f, "system"),
        }
    }
}

/// A parsed `tool@spec` request with optional backend-specific options
/// (e.g. java distribution, rust profile).
#[derive(Debug, Clone)]
pub struct ToolRequest {
    pub backend: String,
    pub spec: VersionSpec,
    pub options: BTreeMap<String, String>,
}

impl ToolRequest {
    /// Parse and canonicalize `tool[option=value]@selector`. Dynamic namespace
    /// subjects and inline options are validated through their central schema.
    pub fn parse(s: &str) -> Result<ToolRequest> {
        let parsed = crate::tool::ToolSpec::parse(s)?;
        Ok(ToolRequest {
            backend: parsed.id.to_string(),
            spec: VersionSpec::parse(parsed.selector().unwrap_or_default()),
            options: parsed.options.into_map(),
        })
    }
}

/// A resolved concrete version, ready to install/activate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersion {
    pub backend: String,
    pub version: String,
    pub options: BTreeMap<String, String>,
}

impl ToolVersion {
    pub fn new(backend: impl Into<String>, version: impl Into<String>) -> ToolVersion {
        ToolVersion {
            backend: backend.into(),
            version: version.into(),
            options: BTreeMap::new(),
        }
    }
}

impl fmt::Display for ToolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.backend, self.version)
    }
}

/// Select the best matching version string from a candidate list for a spec.
///
/// `candidates` should be sorted ascending (oldest first). `is_stable` and
/// `is_lts` help resolve `latest`/`lts`. Returns the chosen version.
pub fn select_version<'a>(
    spec: &VersionSpec,
    candidates: &'a [VersionInfo],
) -> Option<&'a VersionInfo> {
    match spec {
        VersionSpec::System => None,
        VersionSpec::Latest => candidates.iter().rev().find(|v| v.stable),
        VersionSpec::Lts(None) => candidates.iter().rev().find(|v| v.lts.is_some()),
        VersionSpec::Lts(Some(line)) => candidates.iter().rev().find(|v| {
            v.lts
                .as_deref()
                .map(|l| l.eq_ignore_ascii_case(line))
                .unwrap_or(false)
        }),
        VersionSpec::Exact(want) => candidates.iter().find(|v| v.version == *want),
        VersionSpec::Range(requirement) => {
            let requirements = npm_range_requirements(requirement).ok()?;
            candidates.iter().rev().find(|candidate| {
                candidate.stable
                    && semver::Version::parse(candidate.version.trim_start_matches('v'))
                        .map(|version| {
                            requirements
                                .iter()
                                .any(|requirement| requirement.matches(&version))
                        })
                        .unwrap_or(false)
            })
        }
        VersionSpec::Prefix(pfx) => {
            // match versions whose dotted components start with the prefix
            let want = pfx.trim_end_matches('.');
            candidates
                .iter()
                .rev()
                .find(|v| version_has_prefix(&v.version, want))
        }
    }
}

pub fn select_version_with_prerelease<'a>(
    spec: &VersionSpec,
    candidates: &'a [VersionInfo],
    policy: crate::config::PrereleasePolicy,
) -> Option<&'a VersionInfo> {
    use crate::config::PrereleasePolicy;
    let exact_prerelease = matches!(
        spec,
        VersionSpec::Exact(version)
            if semver::Version::parse(version)
                .map(|version| !version.pre.is_empty())
                .unwrap_or(false)
    );
    match policy {
        PrereleasePolicy::Never if exact_prerelease => None,
        PrereleasePolicy::Allow => match spec {
            VersionSpec::Latest => candidates.last(),
            VersionSpec::Prefix(prefix) => {
                let want = prefix.trim_end_matches('.');
                candidates
                    .iter()
                    .rev()
                    .find(|version| version_has_prefix(&version.version, want))
            }
            VersionSpec::Range(requirement) => {
                let requirements = npm_range_requirements(requirement).ok()?;
                candidates.iter().rev().find(|candidate| {
                    semver::Version::parse(candidate.version.trim_start_matches('v'))
                        .map(|version| {
                            requirements
                                .iter()
                                .any(|requirement| requirement.matches(&version))
                        })
                        .unwrap_or(false)
                })
            }
            _ => select_version(spec, candidates),
        },
        PrereleasePolicy::Never | PrereleasePolicy::IfExplicit => {
            if exact_prerelease {
                select_version(spec, candidates)
            } else {
                let stable = candidates
                    .iter()
                    .filter(|candidate| candidate.stable)
                    .cloned()
                    .collect::<Vec<_>>();
                let selected = select_version(spec, &stable)?;
                candidates
                    .iter()
                    .find(|candidate| candidate.version == selected.version)
            }
        }
    }
}

fn version_has_prefix(version: &str, prefix: &str) -> bool {
    if version == prefix {
        return true;
    }
    let v_parts: Vec<&str> = version.split('.').collect();
    let p_parts: Vec<&str> = prefix.split('.').collect();
    if p_parts.len() > v_parts.len() {
        return false;
    }
    v_parts.iter().zip(p_parts.iter()).all(|(a, b)| a == b)
}

/// Metadata about a single installable version.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub version: String,
    pub stable: bool,
    /// LTS line name if this is an LTS release (e.g. `iron`), else None.
    pub lts: Option<String>,
}

impl VersionInfo {
    pub fn stable(version: impl Into<String>) -> VersionInfo {
        VersionInfo {
            version: version.into(),
            stable: true,
            lts: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_specs() {
        assert_eq!(VersionSpec::parse("latest"), VersionSpec::Latest);
        assert_eq!(VersionSpec::parse("lts"), VersionSpec::Lts(None));
        assert_eq!(
            VersionSpec::parse("lts/iron"),
            VersionSpec::Lts(Some("iron".into()))
        );
        assert_eq!(VersionSpec::parse("20"), VersionSpec::Prefix("20".into()));
        assert_eq!(
            VersionSpec::parse("20.11"),
            VersionSpec::Prefix("20.11".into())
        );
        assert_eq!(
            VersionSpec::parse("v20.11.1"),
            VersionSpec::Exact("20.11.1".into())
        );
        assert_eq!(VersionSpec::parse("system"), VersionSpec::System);
    }

    #[test]
    fn parse_tool_request() {
        let r = ToolRequest::parse("node@20").unwrap();
        assert_eq!(r.backend, "node");
        assert_eq!(r.spec, VersionSpec::Prefix("20".into()));

        let r = ToolRequest::parse("go").unwrap();
        assert_eq!(r.backend, "go");
        assert_eq!(r.spec, VersionSpec::Latest);
    }

    #[test]
    fn parse_namespaced_npm_tool_requests() {
        let r = ToolRequest::parse("npm:prettier@3").unwrap();
        assert_eq!(r.backend, "npm:prettier");
        assert_eq!(r.spec, VersionSpec::Prefix("3".into()));

        let r = ToolRequest::parse("npm:@antfu/ni@0.21.12").unwrap();
        assert_eq!(r.backend, "npm:@antfu/ni");
        assert_eq!(r.spec, VersionSpec::Exact("0.21.12".into()));

        let r = ToolRequest::parse("npm:@antfu/ni").unwrap();
        assert_eq!(r.backend, "npm:@antfu/ni");
        assert_eq!(r.spec, VersionSpec::Latest);
    }

    #[test]
    fn parse_namespaced_npm_tool_requests_canonicalizes_case() {
        let r = ToolRequest::parse("npm:Prettier@3").unwrap();
        assert_eq!(r.backend, "npm:prettier");
        assert_eq!(r.spec, VersionSpec::Prefix("3".into()));

        let r = ToolRequest::parse("npm:@Antfu/Ni").unwrap();
        assert_eq!(r.backend, "npm:@antfu/ni");
        assert_eq!(r.spec, VersionSpec::Latest);
    }

    #[test]
    fn parse_inline_dynamic_options_uses_canonical_schema() {
        let request =
            ToolRequest::parse("npm:Prettier[installer=AUBE,allow_builds='Sharp, esbuild']@3")
                .unwrap();
        assert_eq!(request.backend, "npm:prettier");
        assert_eq!(request.spec, VersionSpec::Prefix("3".into()));
        assert_eq!(request.options["installer"], "aube");
        assert_eq!(request.options["allow_builds"], "esbuild,sharp");
    }

    #[test]
    fn parse_github_request_canonicalizes_id_and_preserves_selector() {
        let request =
            ToolRequest::parse("github:Cli/CLI.git[os=darwin,arch=amd64]@2.96.0").unwrap();
        assert_eq!(request.backend, "github:cli/cli");
        assert_eq!(request.spec, VersionSpec::Exact("2.96.0".into()));
        assert_eq!(request.options["os"], "macos");
        assert_eq!(request.options["arch"], "x64");
    }

    #[test]
    fn parse_rejects_unknown_dynamic_namespaces_and_options_early() {
        assert!(matches!(
            ToolRequest::parse("pip:ripgrep@latest"),
            Err(Error::UnknownBackend(_))
        ));
        assert_eq!(
            ToolRequest::parse("cargo:ripgrep@latest").unwrap().backend,
            "cargo:ripgrep"
        );
        assert!(ToolRequest::parse("npm:prettier[token=secret]@3").is_err());
    }

    #[test]
    fn namespaced_npm_parser_keeps_bare_npm_as_the_cli_backend() {
        let cli = ToolRequest::parse("npm").unwrap();
        assert_eq!(cli.backend, "npm");
        assert_eq!(cli.spec, VersionSpec::Latest);

        let package = ToolRequest::parse("npm:npm").unwrap();
        assert_eq!(package.backend, "npm:npm");
        assert_eq!(package.spec, VersionSpec::Latest);
    }

    #[test]
    fn rejects_invalid_namespaced_npm_requests() {
        assert!(ToolRequest::parse("npm:").is_err());
        assert!(ToolRequest::parse("npm:@antfu").is_err());
        assert!(ToolRequest::parse("npm:@antfu/ni/extra").is_err());
        assert!(ToolRequest::parse("npm:foo#bar").is_err());
        assert!(ToolRequest::parse("npm:foo?bar").is_err());
        assert!(ToolRequest::parse("npm:foo%2fbar").is_err());
        assert!(ToolRequest::parse("npm:foo bar").is_err());
        assert!(ToolRequest::parse("npm:foo\tbar").is_err());
        assert!(ToolRequest::parse("npm:foo/bar").is_err());
        assert!(ToolRequest::parse("npm:foo\\bar").is_err());
        assert!(ToolRequest::parse("npm:.").is_err());
        assert!(ToolRequest::parse("npm:..").is_err());
        assert!(ToolRequest::parse("npm:CON").is_err());
        assert!(ToolRequest::parse("npm:@scope/AUX").is_err());
        assert!(ToolRequest::parse(&format!("npm:{}", "a".repeat(215))).is_err());
    }

    #[test]
    fn accepts_safe_scoped_names_and_normalizes_case() {
        let request = ToolRequest::parse("npm:@Scope/Package_Name-1.2").unwrap();
        assert_eq!(request.backend, "npm:@scope/package_name-1.2");
        assert_eq!(request.spec, VersionSpec::Latest);
    }

    #[test]
    fn accepted_npm_names_map_to_safe_inventory_paths() {
        let request = ToolRequest::parse("npm:@Antfu/Ni").unwrap();
        assert_eq!(
            crate::dirs::sanitize_tool_id(&request.backend),
            std::path::PathBuf::from("npm/@antfu/ni")
        );
    }

    fn vi(v: &str, stable: bool, lts: Option<&str>) -> VersionInfo {
        VersionInfo {
            version: v.into(),
            stable,
            lts: lts.map(String::from),
        }
    }

    #[test]
    fn select_prefix_picks_highest_match() {
        let c = vec![
            vi("20.10.0", true, None),
            vi("20.11.0", true, None),
            vi("20.11.1", true, None),
            vi("21.0.0", true, None),
        ];
        let sel = select_version(&VersionSpec::Prefix("20.11".into()), &c).unwrap();
        assert_eq!(sel.version, "20.11.1");
        let sel = select_version(&VersionSpec::Prefix("20".into()), &c).unwrap();
        assert_eq!(sel.version, "20.11.1");
    }

    #[test]
    fn npm_semver_ranges_select_the_highest_stable_match() {
        let candidates = vec![
            vi("18.20.0", true, None),
            vi("20.10.0", true, None),
            vi("22.4.1", true, None),
            vi("23.0.0-beta.1", false, None),
            vi("24.1.0", true, None),
        ];
        let range = VersionSpec::parse_range(">=20 <23").unwrap();
        assert_eq!(
            select_version(&range, &candidates).unwrap().version,
            "22.4.1"
        );
        let alternative = VersionSpec::parse_range("^18.0.0 || >=24").unwrap();
        assert_eq!(
            select_version(&alternative, &candidates).unwrap().version,
            "24.1.0"
        );
        assert!(VersionSpec::parse_range("not a range").is_err());
    }

    #[test]
    fn prerelease_policy_controls_implicit_and_explicit_selection() {
        let candidates = vec![vi("1.0.0", true, None), vi("1.1.0-beta.1", false, None)];
        assert_eq!(
            select_version_with_prerelease(
                &VersionSpec::Latest,
                &candidates,
                crate::config::PrereleasePolicy::IfExplicit,
            )
            .unwrap()
            .version,
            "1.0.0"
        );
        assert_eq!(
            select_version_with_prerelease(
                &VersionSpec::Latest,
                &candidates,
                crate::config::PrereleasePolicy::Allow,
            )
            .unwrap()
            .version,
            "1.1.0-beta.1"
        );
        assert!(select_version_with_prerelease(
            &VersionSpec::Exact("1.1.0-beta.1".into()),
            &candidates,
            crate::config::PrereleasePolicy::Never,
        )
        .is_none());
    }

    #[test]
    fn select_latest_and_lts() {
        let c = vec![
            vi("18.20.0", true, Some("hydrogen")),
            vi("20.11.1", true, Some("iron")),
            vi("21.6.0", true, None),
            vi("22.0.0-nightly", false, None),
        ];
        assert_eq!(
            select_version(&VersionSpec::Latest, &c).unwrap().version,
            "21.6.0"
        );
        assert_eq!(
            select_version(&VersionSpec::Lts(None), &c).unwrap().version,
            "20.11.1"
        );
        assert_eq!(
            select_version(&VersionSpec::Lts(Some("hydrogen".into())), &c)
                .unwrap()
                .version,
            "18.20.0"
        );
    }
}
