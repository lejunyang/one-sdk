//! System package manager integration: read-only discovery.
//!
//! osdk manages SDKs in its own store; it does not manage the host. This
//! subsystem exists for the packages an SDK toolchain needs but osdk has no
//! business owning -- shared libraries, build prerequisites -- which is what
//! the host's own package manager is for.
//!
//! Three boundaries define the subsystem, and all three are deliberate:
//!
//! - **It does not implement `Backend`.** Every backend is instantiated as an
//!   `Arc<dyn Backend>` by `Registry::new`, so each trait method reaches the
//!   vtable and the linker cannot prove it unreachable. `container/` faced the
//!   same question for Docker and containerd and stayed out of the trait; a
//!   system package manager is the same kind of thing -- host infrastructure
//!   osdk talks to, not an SDK osdk installs -- so it is modelled the same way.
//!   Discovery therefore adds nothing to the shim.
//!
//! - **It proxies, it does not reimplement.** osdk asks the installed manager
//!   and reads its answer. It does not fetch bottles, relocate binaries, or
//!   re-sign anything. Measuring the alternative put the cheapest viable
//!   self-implementation 13% over the size budget, and it would not have bought
//!   the one thing that motivated the question, which was control over the
//!   install directory.
//!
//! - **Discovery never writes and never elevates.** This phase runs two
//!   read-only probes.

pub mod apply;
pub mod config;
pub mod distro;
pub mod elevate;
pub mod install;
pub mod mirror;
pub mod report;
pub mod status;
pub mod winget;

pub use apply::{
    apply_plan, assess_feasibility, fingerprint_sources, ApplyOutcome, ApplyRefused, Consequence,
    Infeasible, MirrorPlan, PlannedCommand, RegistrationShape,
};
pub use config::{KeyError, PackageKey, PackageRequest, SyspkgConfig};
pub use distro::{query_installed, DistroManager, DistroReport, RollbackAbility};
pub use elevate::{Elevation, ElevationContext, RefusalReason};
pub use install::{
    explain_install_code, install_succeeded, plan_installs, run_installs, InstallPlan,
    InstallResult, PlannedInstall, SkipReason, SkippedPackage,
};
pub use mirror::{
    acceleration_of, effective_winget_sources, preferred_winget_source, probe_winget_sources,
    source_published_at, winget_probe_url, winget_sources, Acceleration, MirrorCandidate,
    MirrorMeasurement, NoPreferredSource, DEFAULT_WINGET_SOURCE_NAME, WINGET_MIRRORS,
    WINGET_OFFICIAL_ENDPOINT, WINGET_PROBE_FILE, WINGET_SOURCE_TOOL,
};
pub use report::{
    Capability, CapabilityStatus, ManagerDetails, ManagerKind, ManagerReport, ManagerStatus,
    ProbeOutcome, ProbePurpose, ProbeRecord, SourceRecord, SourceTrust, SystemPackageReport,
    WingetDetails, SYSPKG_DIAGNOSTIC_SCHEMA_VERSION,
};
pub use status::{
    evaluate, parse_exported_packages, ExportedPackage, PackageState, PackageStatus, StatusReport,
    SYSPKG_STATUS_SCHEMA_VERSION,
};

use std::time::Duration;

use crate::process::{CaptureLimits, CommandRunner};

/// Wall-clock and byte bounds for one discovery probe.
///
/// Discovery runs while the user waits, so an unresponsive manager has to lose
/// rather than hang. The output bound is generous next to the few kilobytes a
/// source listing occupies, while still refusing to buffer without limit.
pub const DISCOVERY_LIMITS: CaptureLimits =
    CaptureLimits::new(Duration::from_secs(20), 1 << 20, 1 << 16);

/// Inspect every manager that applies to this host.
///
/// Managers that cannot exist on this platform are reported as not applicable
/// rather than omitted, so the output shape does not change with the host and
/// consumers can rely on a fixed set of entries.
pub fn diagnose_all(runner: &dyn CommandRunner) -> SystemPackageReport {
    SystemPackageReport::new(vec![winget::diagnose(runner, DISCOVERY_LIMITS)])
        .with_distro_managers(distro::detect(runner, DISCOVERY_LIMITS))
}

/// Every winget package the host reports as installed, or `None` when winget
/// could not be queried.
///
/// `None` must be reported as unknown rather than as an empty host: "winget is
/// unavailable" is not evidence that a package is absent.
pub fn installed_winget_packages(
    runner: &dyn CommandRunner,
    scratch_directory: &std::path::Path,
) -> Option<Vec<status::ExportedPackage>> {
    winget::installed_packages(runner, DISCOVERY_LIMITS, scratch_directory)
}

/// Whether a distro manager has a package, and at what version.
///
/// `None` means the question went unanswered -- the manager is absent or the
/// probe failed -- which callers must not report as "the package is missing".
pub fn installed_distro_package(
    runner: &dyn CommandRunner,
    manager: distro::DistroManager,
    package: &str,
) -> Option<Option<String>> {
    distro::query_installed(runner, DISCOVERY_LIMITS, manager, package)
}

/// The sources a manager currently has registered.
pub fn registered_sources(runner: &dyn CommandRunner, manager: ManagerKind) -> Vec<SourceRecord> {
    match manager {
        ManagerKind::Winget => winget::registered_sources(runner, DISCOVERY_LIMITS),
        // Homebrew is not wired up yet; an empty list keeps selection honest.
        ManagerKind::Homebrew => Vec::new(),
        // Distro managers have no registered-source concept to enumerate:
        // apt reads a source list, it does not keep a named registry the way
        // winget does. An empty list is the honest answer, not a gap.
        ManagerKind::Distro(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::ExitStatus;

    use super::*;
    use crate::process::{CommandOutcome, CommandSpec};

    struct AbsentRunner;

    impl CommandRunner for AbsentRunner {
        fn run_captured(&self, _command: &CommandSpec, _limits: CaptureLimits) -> CommandOutcome {
            CommandOutcome::NotInstalled
        }

        fn run_foreground(&self, _command: &CommandSpec) -> io::Result<ExitStatus> {
            panic!("discovery must never run a foreground command");
        }
    }

    #[test]
    fn every_known_manager_appears_whatever_the_host() {
        let report = diagnose_all(&AbsentRunner);

        assert_eq!(
            report.managers.len(),
            1,
            "the report shape must not depend on what happens to be installed"
        );
        assert_eq!(report.managers[0].manager, ManagerKind::Winget);
    }

    #[test]
    fn nothing_is_healthy_when_no_manager_is_installed() {
        let report = diagnose_all(&AbsentRunner);

        assert_eq!(report.healthy().count(), 0);
    }

    #[test]
    fn the_probe_deadline_is_bounded() {
        assert!(
            DISCOVERY_LIMITS.timeout <= Duration::from_secs(30),
            "a user is waiting on discovery; it cannot wait indefinitely"
        );
    }
}
