//! Declarative configuration for system packages: `[syspkg]`.
//!
//! Kept in its own top-level table rather than inside `[tools]`, following the
//! decision mise reached and documented: host packages "are deliberately
//! separate from `[tools]`: they are not version-pinned per project, do not get
//! shims, and are managed by the platform's package manager outside the
//! project". Merging them would promise per-project isolation that a
//! machine-wide package manager cannot deliver.
//!
//! # Trust
//!
//! This table can cause software to be installed on the user's machine, so it is
//! execution-affecting project configuration. No special wiring is needed:
//! `trust::requires_trust` already treats every top-level key other than
//! `tools` and `aliases` as requiring trust, so `[syspkg]` is covered by
//! construction rather than by a rule that could drift.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::report::ManagerKind;

/// A package requested in `[syspkg.packages]`.
///
/// The version is a **wish, not a lock**: the semantics are "ask for this when
/// installing", never "hold the host at this version". A machine-wide package
/// manager updates on its own schedule, and `osdk.lock` deliberately does not
/// cover system packages, so promising otherwise would be a lie the
/// implementation cannot keep.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PackageRequest {
    /// Desired version, or `latest`.
    pub version: String,
    /// Restrict this request to one operating system (`windows`, `macos`,
    /// `linux`). Absent means every platform where the manager exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
}

impl PackageRequest {
    /// Whether this request asks for whatever version is current.
    pub fn wants_latest(&self) -> bool {
        self.version.is_empty() || self.version.eq_ignore_ascii_case("latest")
    }
}

/// Accept both spellings a person would naturally write.
///
/// `"winget:Foo" = "latest"` and `"winget:Foo" = { version = "1.2" }` mean the
/// same thing, and rejecting either would be a papercut with no upside.
impl<'de> Deserialize<'de> for PackageRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Version(String),
            Table {
                #[serde(default)]
                version: String,
                #[serde(default)]
                os: Option<String>,
            },
        }

        Ok(match Raw::deserialize(deserializer)? {
            Raw::Version(version) => PackageRequest { version, os: None },
            Raw::Table { version, os } => PackageRequest { version, os },
        })
    }
}

/// A `manager:package-id` key from `[syspkg.packages]`.
///
/// The manager prefix is mandatory. Package ids are not portable -- winget's
/// `PackageIdentifier` is case-sensitive and mirrors a repository path, while
/// brew additionally distinguishes a formula from a cask -- so osdk does not map
/// names across managers and requires the author to say which one they mean.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageKey {
    pub manager: ManagerKind,
    pub id: String,
}

impl PackageKey {
    /// Parse a `manager:package-id` key.
    ///
    /// Splits on the first colon only: a winget identifier may itself contain
    /// colons, and consuming them here would silently address a different
    /// package than the one written down.
    pub fn parse(key: &str) -> Result<Self, KeyError> {
        let Some((manager, id)) = key.split_once(':') else {
            return Err(KeyError::MissingManagerPrefix {
                key: key.to_owned(),
            });
        };
        if id.is_empty() {
            return Err(KeyError::EmptyPackageId {
                key: key.to_owned(),
            });
        }
        let manager = match manager {
            "winget" => ManagerKind::Winget,
            "brew" => ManagerKind::Homebrew,
            other => {
                return Err(KeyError::UnknownManager {
                    manager: other.to_owned(),
                    key: key.to_owned(),
                })
            }
        };
        Ok(Self {
            manager,
            id: id.to_owned(),
        })
    }
}

/// Why a `[syspkg.packages]` key could not be understood.
///
/// Each case is reported rather than skipped. A key osdk cannot parse is a
/// package the user believes is managed, and silently ignoring it would mean
/// reporting "nothing missing" for a host that lacks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyError {
    MissingManagerPrefix { key: String },
    UnknownManager { manager: String, key: String },
    EmptyPackageId { key: String },
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingManagerPrefix { key } => write!(
                formatter,
                "`{key}` needs a manager prefix, for example `winget:{key}`"
            ),
            Self::UnknownManager { manager, key } => write!(
                formatter,
                "`{key}` names an unknown manager `{manager}`; use `winget` or `brew`"
            ),
            Self::EmptyPackageId { key } => {
                write!(formatter, "`{key}` has a manager prefix but no package id")
            }
        }
    }
}

/// The `[syspkg]` table.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyspkgConfig {
    /// Managers allowed to participate. Empty means every manager osdk knows.
    ///
    /// A manager absent from a non-empty list is not used even when the host has
    /// it, which is how a project says "only winget here" without depending on
    /// what the machine happens to have installed.
    pub managers: Vec<String>,
    /// Never attempt to elevate. Print the command to run instead.
    pub no_elevate: bool,
    /// Requested packages, keyed by `manager:package-id`.
    pub packages: BTreeMap<String, PackageRequest>,
}

impl SyspkgConfig {
    /// Whether a manager may be used under this configuration.
    pub fn allows(&self, manager: ManagerKind) -> bool {
        if self.managers.is_empty() {
            return true;
        }
        let name = match manager {
            ManagerKind::Winget => "winget",
            ManagerKind::Homebrew => "brew",
        };
        self.managers.iter().any(|allowed| allowed == name)
    }

    /// Parse every package key, keeping malformed ones as reportable errors.
    ///
    /// Returns both halves rather than failing outright: one unparsable key must
    /// not hide the status of the packages that are written correctly, and it
    /// must not be silently dropped either.
    pub fn parsed_packages(&self) -> (Vec<(PackageKey, PackageRequest)>, Vec<KeyError>) {
        let mut parsed = Vec::new();
        let mut errors = Vec::new();
        for (key, request) in &self.packages {
            match PackageKey::parse(key) {
                Ok(package) => parsed.push((package, request.clone())),
                Err(error) => errors.push(error),
            }
        }
        (parsed, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_without_a_manager_prefix_is_rejected_with_a_usable_hint() {
        let error = PackageKey::parse("ripgrep").unwrap_err();

        assert_eq!(
            error,
            KeyError::MissingManagerPrefix {
                key: "ripgrep".to_owned()
            }
        );
        // The message must show the corrected form, not just state a rule.
        assert!(error.to_string().contains("winget:ripgrep"));
    }

    #[test]
    fn an_unknown_manager_names_the_valid_ones() {
        let error = PackageKey::parse("chocolatey:git").unwrap_err();

        let message = error.to_string();
        assert!(message.contains("chocolatey"));
        assert!(message.contains("winget"), "got: {message}");
    }

    #[test]
    fn an_identifier_containing_a_colon_keeps_the_rest_of_its_name() {
        // Splitting on every colon would address a different package.
        let key = PackageKey::parse("winget:Vendor.Tool:beta").unwrap();

        assert_eq!(key.manager, ManagerKind::Winget);
        assert_eq!(key.id, "Vendor.Tool:beta");
    }

    #[test]
    fn a_prefix_with_no_package_id_is_rejected() {
        assert_eq!(
            PackageKey::parse("winget:").unwrap_err(),
            KeyError::EmptyPackageId {
                key: "winget:".to_owned()
            }
        );
    }

    #[test]
    fn winget_identifiers_stay_case_sensitive() {
        // winget's PackageIdentifier mirrors a repository path and is
        // case-sensitive; normalising it here would fail to find the package.
        let key = PackageKey::parse("winget:BurntSushi.ripgrep.MSVC").unwrap();

        assert_eq!(key.id, "BurntSushi.ripgrep.MSVC");
    }

    #[test]
    fn an_empty_manager_list_allows_every_manager() {
        let config = SyspkgConfig::default();

        assert!(config.allows(ManagerKind::Winget));
        assert!(config.allows(ManagerKind::Homebrew));
    }

    #[test]
    fn a_manager_left_out_of_the_list_is_not_used_even_where_it_exists() {
        let config = SyspkgConfig {
            managers: vec!["winget".to_owned()],
            ..SyspkgConfig::default()
        };

        assert!(config.allows(ManagerKind::Winget));
        assert!(
            !config.allows(ManagerKind::Homebrew),
            "an unlisted manager must stay unused regardless of the host"
        );
    }

    #[test]
    fn latest_is_recognised_however_it_is_spelled() {
        for spelling in ["latest", "LATEST", "Latest", ""] {
            let request = PackageRequest {
                version: spelling.to_owned(),
                os: None,
            };
            assert!(request.wants_latest(), "{spelling:?} means latest");
        }
    }

    #[test]
    fn a_concrete_version_is_not_latest() {
        let request = PackageRequest {
            version: "0.101.0".to_owned(),
            os: None,
        };

        assert!(!request.wants_latest());
    }

    #[test]
    fn one_malformed_key_does_not_hide_the_valid_ones() {
        let mut packages = BTreeMap::new();
        packages.insert(
            "winget:Git.Git".to_owned(),
            PackageRequest {
                version: "latest".to_owned(),
                os: None,
            },
        );
        packages.insert("oops-no-prefix".to_owned(), PackageRequest::default());
        let config = SyspkgConfig {
            packages,
            ..SyspkgConfig::default()
        };

        let (parsed, errors) = config.parsed_packages();

        assert_eq!(parsed.len(), 1, "the valid key must survive");
        assert_eq!(errors.len(), 1, "and the bad one must still be reported");
    }
}
