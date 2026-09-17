//! Reporting the host against `[syspkg.packages]`, without installing anything.
//!
//! # Why `winget export --include-versions` rather than `winget list`
//!
//! `list` prints a table whose headers are localized -- on a Chinese host they
//! read 「名称 / ID / 版本 / 可用 / 源」 -- so a parser written against English
//! headers does not fail there, it silently finds no column and concludes every
//! package is absent. That is worse than crashing.
//!
//! `winget export --include-versions` writes schema 2.0 JSON whose keys are
//! English regardless of display language, carrying exactly the two facts a
//! status report needs: `PackageIdentifier` and `Version`. Verified on a real
//! host: `Git.Git` reports `2.46.0` through both paths, and the JSON is the one
//! that does not depend on the interface language.
//!
//! Exit codes remain the language-independent signal for presence alone:
//! `list --id X --exact` exits 0 when installed and `0x8A150014` when not.

use serde::{Deserialize, Serialize};

use super::config::{PackageKey, PackageRequest};
use super::report::ManagerKind;

/// Schema version for `osdk pkg status --json`.
///
/// Bumped when a consumer would have to change; the field names and enum values
/// never vary with display language.
pub const SYSPKG_STATUS_SCHEMA_VERSION: u32 = 1;

/// Where a requested package stands on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageState {
    /// Installed, and the version satisfies the request.
    Satisfied,
    /// Installed, but at a version other than the one requested.
    ///
    /// Reported, never "corrected": the version in `[syspkg.packages]` is a wish
    /// for install time, and a machine-wide manager is entitled to have moved on.
    VersionDiffers,
    /// Not installed.
    Missing,
    /// This request does not apply to the current platform.
    NotApplicable,
    /// The manager itself is unavailable, so nothing can be said about the
    /// package.
    ///
    /// Distinct from `Missing`: "winget is not here" is not evidence that a
    /// package is absent, and treating it as such would send a user installing
    /// something they may already have.
    ManagerUnavailable,
}

impl PackageState {
    /// Whether this state should make `--missing` fail.
    ///
    /// `VersionDiffers` does not count: the package is present, and the config
    /// never promised to hold a version. `ManagerUnavailable` does not count
    /// either -- it is an unknown, and failing a CI check on an unknown would
    /// make the check mean something different from what it says.
    pub const fn is_missing(self) -> bool {
        matches!(self, Self::Missing)
    }
}

/// One line of a status report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PackageStatus {
    pub manager: ManagerKind,
    pub id: String,
    /// The version asked for, verbatim.
    pub requested: String,
    /// The version observed on the host, when it could be observed.
    pub installed: Option<String>,
    pub state: PackageState,
}

/// A whole status report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub packages: Vec<PackageStatus>,
    /// Keys that could not be parsed, rendered for display.
    ///
    /// Carried in the report rather than logged: a key osdk cannot read is a
    /// package the user believes is managed, so "nothing missing" would be a
    /// false statement while it exists.
    pub invalid_keys: Vec<String>,
}

impl StatusReport {
    pub fn new(packages: Vec<PackageStatus>, invalid_keys: Vec<String>) -> Self {
        Self {
            schema_version: SYSPKG_STATUS_SCHEMA_VERSION,
            packages,
            invalid_keys,
        }
    }

    /// Whether anything requested is absent.
    pub fn has_missing(&self) -> bool {
        self.packages.iter().any(|p| p.state.is_missing())
    }
}

/// One entry of `winget export --include-versions`.
#[derive(Debug, Deserialize)]
pub struct ExportedPackage {
    #[serde(rename = "PackageIdentifier")]
    pub identifier: String,
    /// Absent when exported without `--include-versions`.
    #[serde(rename = "Version")]
    pub version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExportedSourceBlock {
    #[serde(rename = "Packages", default)]
    packages: Vec<ExportedPackage>,
}

#[derive(Debug, Deserialize)]
struct ExportDocument {
    #[serde(rename = "Sources", default)]
    sources: Vec<ExportedSourceBlock>,
}

/// Parse the installed-package list from a `winget export` document.
///
/// Every source block is merged, because a package can come from `winget` or
/// `msstore` and a status report cares that it is present, not which index
/// served it. An unparsable document yields an empty list, which callers must
/// treat as "unknown" rather than "nothing installed".
pub fn parse_exported_packages(json: &str) -> Vec<ExportedPackage> {
    let Ok(document) = serde_json::from_str::<ExportDocument>(json) else {
        return Vec::new();
    };
    document
        .sources
        .into_iter()
        .flat_map(|source| source.packages)
        .collect()
}

/// Compare one request against the installed set.
///
/// `installed` is `None` when the manager could not be queried at all, which is
/// reported as [`PackageState::ManagerUnavailable`] rather than as missing.
pub fn evaluate(
    key: &PackageKey,
    request: &PackageRequest,
    installed: Option<&[ExportedPackage]>,
    platform: &crate::platform::Platform,
) -> PackageStatus {
    let requested = if request.wants_latest() {
        "latest".to_owned()
    } else {
        request.version.clone()
    };

    // A filter that does not match makes the entry inapplicable rather than
    // missing, so a Windows-only package is not reported as a gap on Linux.
    if !request.applies_to(platform) {
        return PackageStatus {
            manager: key.manager,
            id: key.id.clone(),
            requested,
            installed: None,
            state: PackageState::NotApplicable,
        };
    }

    let Some(installed) = installed else {
        return PackageStatus {
            manager: key.manager,
            id: key.id.clone(),
            requested,
            installed: None,
            state: PackageState::ManagerUnavailable,
        };
    };

    // winget's PackageIdentifier is case-sensitive and mirrors a repository
    // path, so it is matched exactly.
    let found = installed
        .iter()
        .find(|package| package.identifier == key.id);

    let Some(found) = found else {
        return PackageStatus {
            manager: key.manager,
            id: key.id.clone(),
            requested,
            installed: None,
            state: PackageState::Missing,
        };
    };

    let state = if request.wants_latest() {
        // "latest" is satisfied by whatever is present: osdk cannot know what
        // the newest version is without asking the network, and a status report
        // must stay offline.
        PackageState::Satisfied
    } else {
        match found.version.as_deref() {
            Some(version) if version == request.version => PackageState::Satisfied,
            // A pinned request with an unknown installed version cannot be
            // called satisfied, but the package is present, so it is not
            // missing either.
            _ => PackageState::VersionDiffers,
        }
    };

    PackageStatus {
        manager: key.manager,
        id: key.id.clone(),
        requested,
        installed: found.version.clone(),
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Arch, Libc, Os, Platform, PlatformFilter};

    /// A concrete host to evaluate against. Tests name the platform explicitly
    /// rather than using `Platform::current()`, so a filter test asserts the
    /// same thing on every machine that runs it.
    fn windows() -> Platform {
        Platform {
            os: Os::Windows,
            arch: Arch::X64,
            libc: Libc::None,
        }
    }

    /// The shape a real host emitted, trimmed to the fields that matter.
    const REAL_EXPORT: &str = r#"{
      "$schema": "https://aka.ms/winget-packages.schema.2.0.json",
      "Sources": [
        {
          "Packages": [
            { "PackageIdentifier": "Git.Git", "Version": "2.46.0" },
            { "PackageIdentifier": "7zip.7zip", "Version": "22.01" }
          ],
          "SourceDetails": { "Name": "winget" }
        },
        {
          "Packages": [
            { "PackageIdentifier": "9NKSQGP7F2NH" }
          ],
          "SourceDetails": { "Name": "msstore" }
        }
      ]
    }"#;

    fn request(version: &str) -> PackageRequest {
        PackageRequest {
            version: version.to_owned(),
            platform: Default::default(),
        }
    }

    fn key(id: &str) -> PackageKey {
        PackageKey {
            manager: ManagerKind::Winget,
            id: id.to_owned(),
        }
    }

    #[test]
    fn parses_the_export_a_real_host_emitted_across_every_source() {
        let packages = parse_exported_packages(REAL_EXPORT);

        assert_eq!(packages.len(), 3, "both source blocks must be merged");
        let git = packages
            .iter()
            .find(|p| p.identifier == "Git.Git")
            .expect("Git.Git");
        assert_eq!(git.version.as_deref(), Some("2.46.0"));
    }

    #[test]
    fn a_store_package_without_a_version_still_counts_as_installed() {
        let installed = parse_exported_packages(REAL_EXPORT);

        let status = evaluate(
            &key("9NKSQGP7F2NH"),
            &request("latest"),
            Some(&installed),
            &windows(),
        );

        assert_eq!(status.state, PackageState::Satisfied);
        assert_eq!(status.installed, None, "no version was exported for it");
    }

    #[test]
    fn a_malformed_document_yields_nothing_rather_than_panicking() {
        assert!(parse_exported_packages("not json").is_empty());
        assert!(parse_exported_packages("{}").is_empty());
    }

    #[test]
    fn an_installed_package_satisfies_a_latest_request() {
        let installed = parse_exported_packages(REAL_EXPORT);

        let status = evaluate(
            &key("Git.Git"),
            &request("latest"),
            Some(&installed),
            &windows(),
        );

        assert_eq!(status.state, PackageState::Satisfied);
        assert_eq!(status.installed.as_deref(), Some("2.46.0"));
        assert!(!status.state.is_missing());
    }

    #[test]
    fn an_exact_version_match_is_satisfied() {
        let installed = parse_exported_packages(REAL_EXPORT);

        let status = evaluate(
            &key("Git.Git"),
            &request("2.46.0"),
            Some(&installed),
            &windows(),
        );

        assert_eq!(status.state, PackageState::Satisfied);
    }

    #[test]
    fn a_different_version_is_reported_but_not_treated_as_missing() {
        let installed = parse_exported_packages(REAL_EXPORT);

        let status = evaluate(
            &key("Git.Git"),
            &request("2.99.0"),
            Some(&installed),
            &windows(),
        );

        assert_eq!(status.state, PackageState::VersionDiffers);
        assert_eq!(status.installed.as_deref(), Some("2.46.0"));
        assert!(
            !status.state.is_missing(),
            "the package is present; the config never promised to hold a version"
        );
    }

    #[test]
    fn an_absent_package_is_missing() {
        let installed = parse_exported_packages(REAL_EXPORT);

        let status = evaluate(
            &key("Nope.Nope"),
            &request("latest"),
            Some(&installed),
            &windows(),
        );

        assert_eq!(status.state, PackageState::Missing);
        assert!(status.state.is_missing());
    }

    #[test]
    fn an_unqueryable_manager_is_unknown_rather_than_missing() {
        let status = evaluate(&key("Git.Git"), &request("latest"), None, &windows());

        assert_eq!(status.state, PackageState::ManagerUnavailable);
        assert!(
            !status.state.is_missing(),
            "an unavailable manager is not evidence the package is absent"
        );
    }

    #[test]
    fn a_request_for_another_os_is_not_applicable_here() {
        let installed = parse_exported_packages(REAL_EXPORT);
        let macos_only = PackageRequest {
            version: "latest".to_owned(),
            platform: PlatformFilter {
                os: vec![crate::platform::Os::Macos],
                arch: Vec::new(),
            },
        };

        let status = evaluate(&key("Some.Tool"), &macos_only, Some(&installed), &windows());

        assert_eq!(status.state, PackageState::NotApplicable);
        assert!(!status.state.is_missing());
    }

    #[test]
    fn an_os_restriction_matching_this_host_is_still_evaluated() {
        let installed = parse_exported_packages(REAL_EXPORT);
        let windows_only = PackageRequest {
            version: "latest".to_owned(),
            platform: PlatformFilter {
                os: vec![crate::platform::Os::Windows],
                arch: Vec::new(),
            },
        };

        let status = evaluate(&key("Git.Git"), &windows_only, Some(&installed), &windows());

        assert_eq!(
            status.state,
            PackageState::Satisfied,
            "the os name must match case-insensitively"
        );
    }

    #[test]
    fn identifier_matching_is_case_sensitive() {
        let installed = parse_exported_packages(REAL_EXPORT);

        // winget would not find `git.git` either; reporting it as installed
        // would make osdk disagree with the tool it is reporting on.
        let status = evaluate(
            &key("git.git"),
            &request("latest"),
            Some(&installed),
            &windows(),
        );

        assert_eq!(status.state, PackageState::Missing);
    }

    #[test]
    fn a_report_knows_whether_anything_is_missing() {
        let installed = parse_exported_packages(REAL_EXPORT);
        let present = evaluate(
            &key("Git.Git"),
            &request("latest"),
            Some(&installed),
            &windows(),
        );
        let absent = evaluate(
            &key("Nope"),
            &request("latest"),
            Some(&installed),
            &windows(),
        );

        assert!(!StatusReport::new(vec![present.clone()], Vec::new()).has_missing());
        assert!(StatusReport::new(vec![present, absent], Vec::new()).has_missing());
    }

    #[test]
    fn status_json_is_deterministic_and_language_neutral() {
        let installed = parse_exported_packages(REAL_EXPORT);
        let status = evaluate(
            &key("Git.Git"),
            &request("2.99.0"),
            Some(&installed),
            &windows(),
        );
        let report = StatusReport::new(vec![status], vec!["oops".to_owned()]);

        let first = serde_json::to_string(&report).unwrap();
        let second = serde_json::to_string(&report).unwrap();
        assert_eq!(first, second, "two serializations must be byte-identical");
        assert!(first.contains("\"state\":\"version-differs\""));
        assert!(
            first.contains(&format!(
                "\"schema_version\":{SYSPKG_STATUS_SCHEMA_VERSION}"
            )),
            "consumers need the schema version, got: {first}"
        );
    }
}
