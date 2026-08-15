//! Version request / resolution types shared across backends.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::{Error, Result};

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
        if is_exact_semver(core) {
            VersionSpec::Exact(core.to_string())
        } else {
            VersionSpec::Prefix(core.to_string())
        }
    }
}

fn is_exact_semver(s: &str) -> bool {
    // exact = at least major.minor.patch numeric
    let numeric_core = s.split(['-', '+']).next().unwrap_or(s);
    let parts: Vec<&str> = numeric_core.split('.').collect();
    parts.len() >= 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

impl fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionSpec::Latest => write!(f, "latest"),
            VersionSpec::Lts(None) => write!(f, "lts"),
            VersionSpec::Lts(Some(n)) => write!(f, "lts/{n}"),
            VersionSpec::Prefix(p) => write!(f, "{p}"),
            VersionSpec::Exact(v) => write!(f, "{v}"),
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
    /// Parse `node`, `node@20`, `node@lts`, `java@temurin-21` (distribution
    /// carried as an option is backend-specific; here we keep the raw spec).
    pub fn parse(s: &str) -> Result<ToolRequest> {
        let (backend, ver) = match s.split_once('@') {
            Some((b, v)) => (b.trim(), v.trim()),
            None => (s.trim(), ""),
        };
        if backend.is_empty() {
            return Err(Error::other(format!("invalid tool request `{s}`")));
        }
        Ok(ToolRequest {
            backend: backend.to_string(),
            spec: VersionSpec::parse(ver),
            options: BTreeMap::new(),
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
        VersionSpec::Lts(Some(line)) => candidates
            .iter()
            .rev()
            .find(|v| v.lts.as_deref().map(|l| l.eq_ignore_ascii_case(line)).unwrap_or(false)),
        VersionSpec::Exact(want) => candidates.iter().find(|v| v.version == *want),
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
        assert_eq!(VersionSpec::parse("lts/iron"), VersionSpec::Lts(Some("iron".into())));
        assert_eq!(VersionSpec::parse("20"), VersionSpec::Prefix("20".into()));
        assert_eq!(VersionSpec::parse("20.11"), VersionSpec::Prefix("20.11".into()));
        assert_eq!(VersionSpec::parse("v20.11.1"), VersionSpec::Exact("20.11.1".into()));
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

    fn vi(v: &str, stable: bool, lts: Option<&str>) -> VersionInfo {
        VersionInfo { version: v.into(), stable, lts: lts.map(String::from) }
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
    fn select_latest_and_lts() {
        let c = vec![
            vi("18.20.0", true, Some("hydrogen")),
            vi("20.11.1", true, Some("iron")),
            vi("21.6.0", true, None),
            vi("22.0.0-nightly", false, None),
        ];
        assert_eq!(select_version(&VersionSpec::Latest, &c).unwrap().version, "21.6.0");
        assert_eq!(select_version(&VersionSpec::Lts(None), &c).unwrap().version, "20.11.1");
        assert_eq!(
            select_version(&VersionSpec::Lts(Some("hydrogen".into())), &c).unwrap().version,
            "18.20.0"
        );
    }
}
