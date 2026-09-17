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
use crate::platform::{Platform, PlatformFilter};

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
    /// Restrict this request to some operating systems and/or architectures.
    ///
    /// An unrestricted filter means "every platform where the manager exists",
    /// which is what an entry without `os`/`arch` gets.
    #[serde(skip_serializing_if = "PlatformFilter::is_unrestricted")]
    #[serde(serialize_with = "serialize_filter")]
    pub platform: PlatformFilter,
}

/// Serialize the filter back as the `os`/`arch` token lists it was written as.
fn serialize_filter<S: serde::Serializer>(
    filter: &PlatformFilter,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(None)?;
    if !filter.os.is_empty() {
        let tokens: Vec<_> = filter.os.iter().map(|os| os.config_token()).collect();
        map.serialize_entry("os", &tokens)?;
    }
    if !filter.arch.is_empty() {
        let tokens: Vec<_> = filter.arch.iter().map(|arch| arch.config_token()).collect();
        map.serialize_entry("arch", &tokens)?;
    }
    map.end()
}

impl PackageRequest {
    /// Whether this request asks for whatever version is current.
    pub fn wants_latest(&self) -> bool {
        self.version.is_empty() || self.version.eq_ignore_ascii_case("latest")
    }

    /// Whether this request applies to `platform`.
    pub fn applies_to(&self, platform: &Platform) -> bool {
        self.platform.matches(platform)
    }
}

/// Accept both spellings a person would naturally write.
///
/// `"winget:Foo" = "latest"` and `"winget:Foo" = { version = "1.2" }` mean the
/// same thing, and rejecting either would be a papercut with no upside.
///
/// `os` and `arch` each accept a single token or a list. An unrecognized token
/// is a hard error here rather than a filter that never matches: the previous
/// behavior compared `os` as a bare string, so `os = "windwos"` silently made
/// the package inapplicable on every host while the status report still said
/// `not applicable`, which reads exactly like a correct restriction.
impl<'de> Deserialize<'de> for PackageRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Tokens {
            One(String),
            Many(Vec<String>),
        }

        impl Tokens {
            fn into_vec(self) -> Vec<String> {
                match self {
                    Tokens::One(value) => vec![value],
                    Tokens::Many(values) => values,
                }
            }
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Version(String),
            Table {
                #[serde(default)]
                version: String,
                #[serde(default)]
                os: Option<Tokens>,
                #[serde(default)]
                arch: Option<Tokens>,
            },
        }

        Ok(match Raw::deserialize(deserializer)? {
            Raw::Version(version) => PackageRequest {
                version,
                platform: PlatformFilter::default(),
            },
            Raw::Table { version, os, arch } => {
                let os = os.map(Tokens::into_vec).unwrap_or_default();
                let arch = arch.map(Tokens::into_vec).unwrap_or_default();
                let platform = PlatformFilter::parse(&os, &arch)
                    .map_err(<D::Error as serde::de::Error>::custom)?;
                PackageRequest { version, platform }
            }
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
            "apt" => ManagerKind::Distro(super::distro::DistroManager::Apt),
            "apk" => ManagerKind::Distro(super::distro::DistroManager::Apk),
            "pacman" => ManagerKind::Distro(super::distro::DistroManager::Pacman),
            "dnf" => ManagerKind::Distro(super::distro::DistroManager::Dnf),
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
                "`{key}` names an unknown manager `{manager}`; use one of \
                 winget, brew, apt, apk, pacman, dnf"
            ),
            Self::EmptyPackageId { key } => {
                write!(formatter, "`{key}` has a manager prefix but no package id")
            }
        }
    }
}

/// The `[syspkg]` table.
///
/// `Default` is written out rather than derived because `mirrors` defaults to
/// `true`: a derived `bool` would be `false`, which would disable acceleration
/// for every project that does not mention it, while still reporting success.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Use a mirror when installing, where the manager supports doing so
    /// without modifying system configuration.
    ///
    /// On by default. Acceleration here is per invocation and leaves no trace,
    /// so there is nothing to undo and no reason to make it opt-in.
    pub mirrors: bool,
    /// Requested packages, keyed by `manager:package-id`.
    pub packages: BTreeMap<String, PackageRequest>,
}

impl Default for SyspkgConfig {
    fn default() -> Self {
        Self {
            managers: Vec::new(),
            no_elevate: false,
            mirrors: true,
            packages: BTreeMap::new(),
        }
    }
}

impl SyspkgConfig {
    /// Whether a manager may be used under this configuration.
    pub fn allows(&self, manager: ManagerKind) -> bool {
        if self.managers.is_empty() {
            return true;
        }
        // `id()` is the same spelling used in configuration keys, so the two
        // cannot drift apart into a list that silently matches nothing.
        self.managers.iter().any(|allowed| allowed == manager.id())
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
    use crate::platform::{Arch, Libc, Os};

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
                platform: Default::default(),
            };
            assert!(request.wants_latest(), "{spelling:?} means latest");
        }
    }

    #[test]
    fn a_concrete_version_is_not_latest() {
        let request = PackageRequest {
            version: "0.101.0".to_owned(),
            platform: Default::default(),
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
                platform: Default::default(),
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

    /// `os` and `arch` both filter, in either the single-token or list spelling,
    /// and the two dimensions are AND rather than OR.
    ///
    /// N=2 matters here: with a single entry a bug that accepts everything and a
    /// bug that accepts nothing both look plausible, so each case asserts one
    /// entry that must match alongside one that must not.
    #[test]
    fn os_and_arch_filter_independently_and_together() {
        let win_arm = Platform {
            os: Os::Windows,
            arch: Arch::Arm64,
            libc: Libc::None,
        };
        let win_x64 = Platform {
            os: Os::Windows,
            arch: Arch::X64,
            libc: Libc::None,
        };
        let linux_arm = Platform {
            os: Os::Linux,
            arch: Arch::Arm64,
            libc: Libc::None,
        };

        let parse = |toml_body: &str| -> PackageRequest {
            #[derive(Deserialize)]
            struct Wrapper {
                package: PackageRequest,
            }
            toml::from_str::<Wrapper>(toml_body).unwrap().package
        };

        // Both dimensions given: only the exact combination applies.
        let both = parse("package = { version = \"1\", os = \"windows\", arch = \"arm64\" }");
        assert!(both.applies_to(&win_arm));
        assert!(!both.applies_to(&win_x64), "arch must also match");
        assert!(!both.applies_to(&linux_arm), "os must also match");

        // Only `arch`: every OS with that architecture applies.
        let arch_only = parse("package = { version = \"1\", arch = \"arm64\" }");
        assert!(arch_only.applies_to(&win_arm));
        assert!(arch_only.applies_to(&linux_arm));
        assert!(!arch_only.applies_to(&win_x64));

        // Lists are OR within one dimension.
        let list = parse("package = { version = \"1\", arch = [\"arm64\", \"x64\"] }");
        assert!(list.applies_to(&win_arm));
        assert!(list.applies_to(&win_x64));

        // Aliases resolve through the same parser the backends already use.
        let aliased = parse("package = { version = \"1\", os = \"win\", arch = \"aarch64\" }");
        assert!(aliased.applies_to(&win_arm));
        assert!(!aliased.applies_to(&linux_arm));

        // No filter at all still means everywhere.
        let bare = parse("package = \"latest\"");
        assert!(bare.applies_to(&win_arm));
        assert!(bare.applies_to(&linux_arm));
        assert!(bare.platform.is_unrestricted());
    }

    /// A misspelled token must be an error, not a filter that never matches.
    ///
    /// This is the failure this feature most needed to prevent. `os` used to be
    /// compared as a bare string, so `os = "windwos"` made the package
    /// inapplicable on every host while `pkg status` still reported it as
    /// `not applicable` -- indistinguishable from a correct restriction, and the
    /// package simply never installed anywhere.
    #[test]
    fn a_misspelled_platform_token_is_rejected_rather_than_never_matching() {
        for body in [
            "package = { version = \"1\", os = \"windwos\" }",
            "package = { version = \"1\", arch = \"arm65\" }",
            "package = { version = \"1\", os = [\"linux\", \"solaris\"] }",
        ] {
            #[derive(Debug, Deserialize)]
            struct Wrapper {
                #[allow(dead_code)]
                package: PackageRequest,
            }
            let error =
                toml::from_str::<Wrapper>(body).expect_err(&format!("should be rejected: {body}"));
            let message = error.to_string();
            // The message has to name the offending token and the accepted set,
            // or the author still has to guess which of the two fields is wrong.
            assert!(
                message.contains("windwos")
                    || message.contains("arm65")
                    || message.contains("solaris"),
                "{message}"
            );
            assert!(
                message.contains("expected one of"),
                "message must list the accepted tokens: {message}"
            );
        }
    }
}
