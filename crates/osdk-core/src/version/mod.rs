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
    /// A version pinned verbatim with a leading `=`, e.g. `=android-36`.
    ///
    /// Distinct from [`VersionSpec::Exact`], which is what a *fully-specified
    /// semver* parses to and which still falls back to looser tiers when no
    /// literal match exists (see [`select_exact`]). `Pinned` never falls back:
    /// the request either names a published version character-for-character or
    /// it does not resolve.
    ///
    /// This exists because some catalogues use identifiers that are not versions
    /// and are not mutually exclusive under prefix matching. Android platform
    /// packages are the case that forced it: `android-36` and `android-36.1` are
    /// two different API levels, and dotted-component prefix matching makes the
    /// former match the latter, so there was previously no way to ask for
    /// exactly `android-36`.
    Pinned(String),
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
        // A leading `=` pins the remainder verbatim. Checked before the semver
        // classification below so `=1.2.3` is a pin rather than an Exact that
        // would still fall back to looser tiers. Case is preserved because the
        // pinned text is compared to published version strings, some of which
        // are mixed-case identifiers (`android-CANARY`).
        if let Some(rest) = s.strip_prefix('=') {
            let rest = rest.trim();
            if !rest.is_empty() {
                return VersionSpec::Pinned(rest.to_string());
            }
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
            // Round-trips through `parse`, so a pin echoed into a lock file or
            // an error message re-reads as the same pin.
            VersionSpec::Pinned(v) => write!(f, "={v}"),
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
        VersionSpec::Exact(want) => select_exact(want, candidates),
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
        VersionSpec::Pinned(want) => candidates.iter().rev().find(|v| v.version == *want),
        VersionSpec::Prefix(pfx) => {
            let want = pfx.trim_end_matches('.');
            // An identifier that names a candidate outright wins over the
            // dotted-component prefix scan below.
            //
            // Prefix matching is component-wise, so `android-36` is a prefix of
            // `android-36.1` and the newest-first scan returned the latter --
            // even though a package literally called `android-36` exists and is
            // a *different* API level. The same shape appears wherever a
            // catalogue's identifiers are not mutually exclusive under prefix
            // matching.
            //
            // Ordinary version prefixes are unaffected: `20` is not the literal
            // version of any Node release, so no candidate matches here and
            // resolution proceeds to the scan as before.
            if let Some(found) = candidates.iter().rev().find(|v| v.version == want) {
                return Some(found);
            }
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
    // A pin names one published version character-for-character, which is the
    // clearest form of "explicit" there is; it must be able to reach a
    // pre-release without also widening the policy for anything else. Unlike
    // `Exact`, this does not require the text to be parseable semver: the
    // catalogues that need pinning use identifiers that are not (`android-36`,
    // `android-37.2-beta3`).
    let pinned = matches!(spec, VersionSpec::Pinned(_));
    let exact_prerelease = pinned
        || matches!(
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
                // Same exact-identifier precedence as `select_version`; kept in
                // step so the two policies cannot disagree about which candidate
                // `android-36` names.
                if let Some(found) = candidates.iter().rev().find(|v| v.version == want) {
                    return Some(found);
                }
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

/// Match an exact request against candidates, scanning newest first.
///
/// Match tiers, in order:
/// 1. literal string equality;
/// 2. semver core equality, ignoring build metadata — `21.0.12` matches
///    `21.0.12+8` (and prerelease tags must agree);
/// 3. dotted-component prefix — `21.0.12` matches a four-part `21.0.12.1+1`
///    PSU when no release with the same semver core exists.
///
/// Tiers 2-3 are required for Java, whose published versions carry build
/// numbers (`+8`) and, for Patch Set Updates, a fourth numeric component that
/// is not valid semver. Strict-semver toolchains (node, go, ...) only ever
/// reach tier 1 because their candidate versions never have a fourth part.
fn select_exact<'a>(want: &str, candidates: &'a [VersionInfo]) -> Option<&'a VersionInfo> {
    if let Some(found) = candidates.iter().rev().find(|v| v.version == want) {
        return Some(found);
    }
    if let Ok(want_version) = semver::Version::parse(want.trim_start_matches('v')) {
        if let Some(found) = candidates.iter().rev().find(|v| {
            semver::Version::parse(v.version.trim_start_matches('v'))
                .map(|candidate| {
                    candidate.major == want_version.major
                        && candidate.minor == want_version.minor
                        && candidate.patch == want_version.patch
                        && candidate.pre == want_version.pre
                })
                .unwrap_or(false)
        }) {
            return Some(found);
        }
    }
    let prefix = want.trim_end_matches('.');
    candidates
        .iter()
        .rev()
        .find(|v| version_has_prefix(&v.version, prefix))
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
            ToolRequest::parse("npm:Prettier[installer=PNPM,allow_builds='Sharp, esbuild']@3")
                .unwrap();
        assert_eq!(request.backend, "npm:prettier");
        assert_eq!(request.spec, VersionSpec::Prefix("3".into()));
        assert_eq!(request.options["installer"], "pnpm");
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
    fn select_exact_ignores_build_metadata_and_prefers_same_core() {
        let c = vec![
            vi("21.0.12+8", true, None),
            vi("21.0.12.1+1", true, None), // four-part PSU, not valid semver
            vi("21.0.13+4", true, None),
        ];
        // `21.0.12` is Exact; it must find the same-core `21.0.12+8` rather
        // than erroring or jumping to the four-part PSU.
        let sel = select_version(&VersionSpec::Exact("21.0.12".into()), &c).unwrap();
        assert_eq!(sel.version, "21.0.12+8");
    }

    #[test]
    fn select_exact_picks_highest_build_of_the_same_core() {
        let c = vec![vi("21.0.12+7", true, None), vi("21.0.12+8", true, None)];
        let sel = select_version(&VersionSpec::Exact("21.0.12".into()), &c).unwrap();
        assert_eq!(sel.version, "21.0.12+8");
    }

    #[test]
    fn select_exact_falls_back_to_dotted_prefix_for_four_part_versions() {
        // Only a PSU exists; the dotted-prefix tier still locates it.
        let c = vec![vi("21.0.12.1+1", true, None), vi("21.0.13+4", true, None)];
        let sel = select_version(&VersionSpec::Exact("21.0.12".into()), &c).unwrap();
        assert_eq!(sel.version, "21.0.12.1+1");
    }

    #[test]
    fn select_exact_matches_prerelease_build_and_rejects_unrelated() {
        let c = vec![
            vi("1.0.0-beta.1+sha.abc", false, None),
            vi("1.0.0", true, None),
            vi("2.0.0", true, None),
        ];
        let sel = select_version(&VersionSpec::Exact("1.0.0-beta.1".into()), &c).unwrap();
        assert_eq!(sel.version, "1.0.0-beta.1+sha.abc");
        assert!(select_version(&VersionSpec::Exact("3.0.0".into()), &c).is_none());
    }

    /// An identifier that names a candidate outright must beat a longer
    /// prefix-compatible sibling.
    ///
    /// Component-wise prefix matching makes `android-36` a prefix of
    /// `android-36.1`, and the newest-first scan therefore returned the latter
    /// even though a package literally called `android-36` exists and is a
    /// different API level. A project needing exactly API 36 silently compiled
    /// against 36.1.
    #[test]
    fn exact_identifier_beats_a_longer_prefix_sibling() {
        // Ordered as the Android manifest orders them: oldest first, so the
        // newest-first scan would reach `android-36.1` before `android-36`.
        let candidates = vec![
            vi("android-35", true, None),
            vi("android-36", true, None),
            vi("android-36.1", true, None),
        ];
        assert_eq!(
            select_version(&VersionSpec::parse("android-36"), &candidates)
                .unwrap()
                .version,
            "android-36"
        );
        // When the same text is NOT a published identifier, the dotted-component
        // scan still runs and still picks the newest match -- the pre-existing
        // behaviour, which this change must not disturb. (Prefix matching is
        // component-wise, so the probe has to be a whole component: `android-3`
        // would match nothing here even before the change.)
        let without_the_base = vec![
            vi("android-36.1", true, None),
            vi("android-36.2", true, None),
        ];
        assert_eq!(
            select_version(&VersionSpec::parse("android-36"), &without_the_base)
                .unwrap()
                .version,
            "android-36.2"
        );
        // And the more specific identifier is still reachable by name.
        assert_eq!(
            select_version(&VersionSpec::parse("android-36.1"), &candidates)
                .unwrap()
                .version,
            "android-36.1"
        );
    }

    /// `36.0` cannot reach API 36, and that is a catalogue fact, not a bug.
    ///
    /// Google publishes `android-36` and `android-36.1`; there is no
    /// `android-36.0`. Under numeric dotted comparison `36.0` is equal to `36`,
    /// but neither is a published identifier of this family (the identifiers all
    /// carry the `android-` namespace), so the only thing that can match is the
    /// prefix scan -- which finds nothing. Asserting this keeps a future "make
    /// 36.0 work" special case from being added silently.
    #[test]
    fn a_bare_numeric_selector_does_not_reach_a_namespaced_identifier() {
        let candidates = vec![vi("android-36", true, None), vi("android-36.1", true, None)];
        assert!(select_version(&VersionSpec::parse("36.0"), &candidates).is_none());
        assert!(select_version(&VersionSpec::parse("36"), &candidates).is_none());
    }

    /// A leading `=` pins verbatim and never falls back to a looser tier.
    #[test]
    fn pinned_specs_match_only_a_literal_published_version() {
        assert_eq!(
            VersionSpec::parse("=android-36"),
            VersionSpec::Pinned("android-36".into())
        );
        // Round-trips, so a pin written into a lock file re-reads as a pin.
        assert_eq!(VersionSpec::parse("=android-36").to_string(), "=android-36");
        // A lone `=` is not a pin; it stays whatever the ordinary rules say.
        assert_eq!(VersionSpec::parse("="), VersionSpec::Prefix("=".into()));

        let candidates = vec![
            vi("android-36", true, None),
            vi("android-36.1", true, None),
            vi("21.0.12.1+1", true, None),
        ];
        assert_eq!(
            select_version(&VersionSpec::parse("=android-36"), &candidates)
                .unwrap()
                .version,
            "android-36"
        );
        // Unlike `Exact`, a pin does not fall back to the dotted-prefix tier:
        // `21.0.12` resolves as Exact but must not resolve as a pin.
        assert_eq!(
            select_version(&VersionSpec::Exact("21.0.12".into()), &candidates)
                .unwrap()
                .version,
            "21.0.12.1+1"
        );
        assert!(select_version(&VersionSpec::parse("=21.0.12"), &candidates).is_none());
        assert!(select_version(&VersionSpec::parse("=android-37"), &candidates).is_none());
    }

    /// A pin is an explicit request, so it may reach a pre-release under the
    /// default policy -- without widening that policy for anything else.
    #[test]
    fn a_pin_counts_as_explicit_for_the_prerelease_policy() {
        let candidates = vec![
            vi("android-37.1", true, None),
            vi("android-37.2-beta3", false, None),
        ];
        for policy in [
            crate::config::PrereleasePolicy::IfExplicit,
            crate::config::PrereleasePolicy::Allow,
        ] {
            assert_eq!(
                select_version_with_prerelease(
                    &VersionSpec::parse("=android-37.2-beta3"),
                    &candidates,
                    policy,
                )
                .unwrap()
                .version,
                "android-37.2-beta3",
                "{policy:?}"
            );
        }
        // A bare `latest` under the default policy still stops at the stable one.
        assert_eq!(
            select_version_with_prerelease(
                &VersionSpec::Latest,
                &candidates,
                crate::config::PrereleasePolicy::IfExplicit,
            )
            .unwrap()
            .version,
            "android-37.1"
        );
        // `never` refuses the pin rather than silently downgrading it.
        assert!(select_version_with_prerelease(
            &VersionSpec::parse("=android-37.2-beta3"),
            &candidates,
            crate::config::PrereleasePolicy::Never,
        )
        .is_none());
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
