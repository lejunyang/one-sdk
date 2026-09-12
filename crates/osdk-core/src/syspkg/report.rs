//! Stable, deterministic, locale-independent diagnostics for system package
//! managers.
//!
//! Every field here is a typed fact or a redacted value. Nothing carries raw
//! output from the managers themselves, because that output is localised: on a
//! Chinese Windows host `winget list` prints its columns as 名称 / ID / 版本,
//! and `winget source list` as 名称 / 参数 / 显式. A report that embedded such
//! text would change shape with the host's display language, which is exactly
//! what machine-readable output must not do.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

/// Version of the `osdk pkg doctor --json` contract.
pub const SYSPKG_DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;

/// A system package manager osdk knows how to inspect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagerKind {
    Winget,
    Homebrew,
}

impl ManagerKind {
    /// The executable name, used for discovery and for the commands printed to
    /// the user. Not localised and not configurable.
    pub const fn program(self) -> &'static str {
        match self {
            Self::Winget => "winget",
            Self::Homebrew => "brew",
        }
    }
}

/// Overall state of one manager on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagerStatus {
    /// Present, responsive, and usable for the read-only operations osdk needs.
    Healthy,
    /// Present and responsive, but something limits what osdk can do with it.
    Degraded,
    /// Not on this host at all.
    NotInstalled,
    /// Not applicable to this platform, so its absence is not a finding.
    NotApplicable,
    /// Present but the operating system refused to run it.
    PermissionDenied,
    /// Present but did not answer within the probe deadline.
    Unresponsive,
    /// Present but too old for the interfaces osdk relies on.
    UnsupportedVersion,
}

/// A capability osdk can establish by read-only discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// The executable can be found and run.
    Client,
    /// Its version could be determined.
    VersionQuery,
    /// Configured sources or taps can be enumerated **structurally**, without
    /// parsing localised tables.
    SourceEnumeration,
    /// Installed packages can be enumerated structurally.
    PackageEnumeration,
    /// Mirror endpoints can be inspected.
    MirrorInspection,
}

/// State of one capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityStatus {
    Supported,
    /// The manager genuinely lacks it.
    Unsupported,
    /// It exists but could not be reached in this pass.
    Unavailable,
    Unknown,
}

/// How osdk obtained a fact, recorded so a surprising report can be retraced.
///
/// This names the *purpose* of a probe rather than its command line, so it
/// stays stable when flags change and cannot leak an argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbePurpose {
    VersionQuery,
    SourceEnumeration,
}

/// Whether a probe answered, and how it failed when it did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeOutcome {
    Succeeded,
    /// Ran and exited non-zero. The code is reported separately, classified.
    Failed,
    NotInstalled,
    PermissionDenied,
    TimedOut,
    SpawnFailed,
}

/// One executed probe, as evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ProbeRecord {
    pub purpose: ProbePurpose,
    pub outcome: ProbeOutcome,
    /// Raw process exit code when the probe ran to completion.
    ///
    /// Kept as a number rather than a message because winget's messages are
    /// localised while its codes are not: a missing package is 0x8A150014 in
    /// every display language.
    pub exit_code: Option<i32>,
}

/// Trust level of a configured source, as reported by the manager itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceTrust {
    /// The manager vouches for this source.
    Trusted,
    /// Explicitly not trusted, or trusted only under a relaxed setting.
    Untrusted,
    /// The manager did not say.
    Unknown,
}

/// A configured package source.
///
/// The endpoint is kept because for system package managers it is a public
/// mirror URL, which is the one fact a user needs to see to tell an official
/// source from a mirror. Sources carrying credentials are not represented here.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SourceRecord {
    /// Stable identifier assigned by the manager, not the display name.
    pub identifier: String,
    /// Name as configured. For winget this is the `Name` field of
    /// `winget source export`, which is an identifier rather than a label and
    /// therefore does not vary with display language.
    pub name: String,
    pub endpoint: Option<String>,
    /// The manager's own type tag, for example `Microsoft.PreIndexed.Package`.
    pub kind: Option<String>,
    pub trust: SourceTrust,
}

/// Manager-specific facts gathered in the same read-only pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ManagerDetails {
    Winget(WingetDetails),
}

/// What osdk learned about winget specifically.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WingetDetails {
    /// Reported client version, for example `v1.29.290`.
    pub version: Option<String>,
    /// Sources configured on this host.
    pub sources: Vec<SourceRecord>,
    /// Whether a non-default source is configured, which changes where
    /// manifests come from.
    pub has_non_default_source: bool,
}

/// The result of inspecting one manager.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ManagerReport {
    pub schema_version: u32,
    pub manager: ManagerKind,
    pub status: ManagerStatus,
    pub details: Option<ManagerDetails>,
    pub capabilities: BTreeMap<Capability, CapabilityStatus>,
    pub probes: BTreeSet<ProbeRecord>,
}

impl ManagerReport {
    pub fn new(manager: ManagerKind, status: ManagerStatus) -> Self {
        Self {
            schema_version: SYSPKG_DIAGNOSTIC_SCHEMA_VERSION,
            manager,
            status,
            details: None,
            capabilities: BTreeMap::new(),
            probes: BTreeSet::new(),
        }
    }

    /// Report a manager that cannot exist on this platform.
    ///
    /// Distinct from `NotInstalled`: winget missing on macOS is not a problem
    /// to fix, so doctor must not advise installing it.
    pub fn not_applicable(manager: ManagerKind) -> Self {
        let mut report = Self::new(manager, ManagerStatus::NotApplicable);
        for capability in [
            Capability::Client,
            Capability::VersionQuery,
            Capability::SourceEnumeration,
            Capability::PackageEnumeration,
            Capability::MirrorInspection,
        ] {
            report.set_capability(capability, CapabilityStatus::Unsupported);
        }
        report
    }

    pub fn set_capability(&mut self, capability: Capability, status: CapabilityStatus) {
        self.capabilities.insert(capability, status);
    }

    pub fn record_probe(&mut self, probe: ProbeRecord) {
        self.probes.insert(probe);
    }

    pub fn set_details(&mut self, details: ManagerDetails) {
        self.details = Some(details);
    }

    /// Whether this report describes something the user could act on.
    ///
    /// A manager that is simply absent from a platform it never belonged to is
    /// not actionable, and neither is a healthy one.
    pub fn is_actionable(&self) -> bool {
        !matches!(
            self.status,
            ManagerStatus::Healthy | ManagerStatus::NotApplicable
        )
    }
}

/// The full read-only diagnosis across every manager osdk inspected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SystemPackageReport {
    pub schema_version: u32,
    pub managers: Vec<ManagerReport>,
}

impl SystemPackageReport {
    pub fn new(managers: Vec<ManagerReport>) -> Self {
        Self {
            schema_version: SYSPKG_DIAGNOSTIC_SCHEMA_VERSION,
            managers,
        }
    }

    /// Managers that are present and usable.
    pub fn healthy(&self) -> impl Iterator<Item = &ManagerReport> {
        self.managers
            .iter()
            .filter(|report| report.status == ManagerStatus::Healthy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manager_absent_from_its_platform_is_not_a_finding() {
        let report = ManagerReport::not_applicable(ManagerKind::Winget);

        assert_eq!(report.status, ManagerStatus::NotApplicable);
        assert!(
            !report.is_actionable(),
            "winget missing on macOS is nothing for the user to fix"
        );
        assert_eq!(
            report.capabilities.get(&Capability::Client),
            Some(&CapabilityStatus::Unsupported)
        );
    }

    #[test]
    fn a_missing_manager_on_its_own_platform_is_a_finding() {
        let report = ManagerReport::new(ManagerKind::Winget, ManagerStatus::NotInstalled);

        assert!(
            report.is_actionable(),
            "winget missing on Windows is worth telling the user about"
        );
    }

    #[test]
    fn report_json_is_deterministic_and_language_neutral() {
        let mut report = ManagerReport::new(ManagerKind::Winget, ManagerStatus::Healthy);
        report.set_capability(Capability::Client, CapabilityStatus::Supported);
        report.set_capability(Capability::VersionQuery, CapabilityStatus::Supported);
        report.record_probe(ProbeRecord {
            purpose: ProbePurpose::VersionQuery,
            outcome: ProbeOutcome::Succeeded,
            exit_code: Some(0),
        });
        report.set_details(ManagerDetails::Winget(WingetDetails {
            version: Some("v1.29.290".to_owned()),
            sources: vec![SourceRecord {
                identifier: "Microsoft.Winget.Source_8wekyb3d8bbwe".to_owned(),
                name: "winget".to_owned(),
                endpoint: Some("https://cdn.winget.microsoft.com/cache".to_owned()),
                kind: Some("Microsoft.PreIndexed.Package".to_owned()),
                trust: SourceTrust::Trusted,
            }],
            has_non_default_source: false,
        }));

        let first = serde_json::to_string(&report).unwrap();
        let second = serde_json::to_string(&report).unwrap();
        assert_eq!(first, second, "serialization must be deterministic");

        // The probe of a Chinese host showed winget rendering its columns as
        // 名称 / ID / 版本 / 可用 / 源. None of that may reach the report: a
        // consumer parsing this JSON must see the same keys and values whatever
        // the host's display language is.
        for localised in ["名称", "版本", "可用", "参数", "显式"] {
            assert!(
                !first.contains(localised),
                "localised text {localised} leaked into machine-readable output"
            );
        }
    }

    #[test]
    fn capabilities_and_probes_serialize_in_a_stable_order() {
        let mut report = ManagerReport::new(ManagerKind::Winget, ManagerStatus::Healthy);
        // Insert out of declaration order; the report must still be stable.
        report.set_capability(Capability::MirrorInspection, CapabilityStatus::Unknown);
        report.set_capability(Capability::Client, CapabilityStatus::Supported);
        report.record_probe(ProbeRecord {
            purpose: ProbePurpose::SourceEnumeration,
            outcome: ProbeOutcome::Succeeded,
            exit_code: Some(0),
        });
        report.record_probe(ProbeRecord {
            purpose: ProbePurpose::VersionQuery,
            outcome: ProbeOutcome::Succeeded,
            exit_code: Some(0),
        });

        let json = serde_json::to_string(&report).unwrap();
        let client_at = json.find("client").expect("client capability");
        let mirror_at = json.find("mirror-inspection").expect("mirror capability");
        assert!(
            client_at < mirror_at,
            "BTreeMap ordering should place client before mirror-inspection"
        );
    }
}
