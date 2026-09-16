//! Read-only detection for Linux distribution package managers.
//!
//! # Why these are detected but never driven
//!
//! apt, apk, pacman and dnf are not "winget on Linux". Three differences make
//! representing them as another manager osdk can apply through the wrong shape:
//!
//! 1. **Every change is global and needs root.** apt's own manual explains that
//!    `full-upgrade` "will remove currently installed packages if this is needed
//!    to upgrade the system as a whole", so one install can cascade into
//!    upgrading shared libraries and removing other packages.
//! 2. **Arch declares partial upgrades unsupported.** The Wiki is explicit:
//!    "never run `pacman -Sy`; instead, always use `pacman -Syu`". Installing
//!    just the one package a project needs *is* a partial upgrade, so the most
//!    natural behaviour for a tool like osdk lands in officially unsupported
//!    territory. It also asks users to read release announcements first, which
//!    cannot be automated.
//! 3. **Failure recovery differs fundamentally across them.** dnf has atomic
//!    `history undo`; pacman can only downgrade by hand from cache, which it
//!    calls a last resort; apt has logs and no undo at all. A single abstraction
//!    cannot promise consistent recovery semantics -- a deeper problem than
//!    merely being hard to implement.
//!
//! So osdk detects, reports, and prints commands the user runs themselves. This
//! matches the stance `container/mirror.rs` already takes toward native
//! runtimes, which it documents as never writing, restarting, recreating or
//! elevating one. A package manager that owns `/usr` warrants it more, not less.
//!
//! # Why the queries here are the ones they are
//!
//! Each is the read-only interface its project documents, chosen so detection
//! never needs root and never parses a localized table:
//!
//! - apt: `dpkg-query -W -f=` with an explicit format string, so the output
//!   shape is osdk's choice rather than a default that could change.
//! - apk: `apk info -e -v`, its documented existence check.
//! - pacman: `pacman -Q`, whose `name version` output is stable and unlocalized.
//! - dnf: `rpm -q --qf` for the same reason as dpkg-query -- an explicit format.

use serde::Serialize;

use crate::process::{CaptureLimits, CommandOutcome, CommandRunner, CommandSpec};

/// A Linux distribution package manager osdk can detect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DistroManager {
    Apt,
    Apk,
    Pacman,
    Dnf,
}

impl DistroManager {
    /// Every manager osdk knows how to detect.
    ///
    /// zypper is deliberately absent. It was not investigated, and the available
    /// clues suggest it may *not* be equivalent to apt -- openSUSE integrates
    /// btrfs snapshots via snapper, which would give filesystem-level rollback,
    /// and zypper documents an exit-code table. Listing it as "same as apt"
    /// would assert something unverified, so it is left out until checked.
    pub const ALL: [DistroManager; 4] = [Self::Apt, Self::Apk, Self::Pacman, Self::Dnf];

    /// The executable whose presence indicates this manager.
    pub const fn program(self) -> &'static str {
        match self {
            Self::Apt => "apt-get",
            Self::Apk => "apk",
            Self::Pacman => "pacman",
            Self::Dnf => "dnf",
        }
    }

    /// Name used in reports and configuration.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Apk => "apk",
            Self::Pacman => "pacman",
            Self::Dnf => "dnf",
        }
    }

    /// How this manager recovers from a failed transaction.
    ///
    /// Reported rather than hidden, because it is the fact that decides whether
    /// a user should let any tool drive their package manager -- and it differs
    /// enough between these four that a single answer would be false.
    pub const fn rollback(self) -> RollbackAbility {
        match self {
            // `dnf history undo` / `rollback`, and rollback is itself atomic.
            Self::Dnf => RollbackAbility::Transactional,
            // Downgrading from the package cache, which pacman itself describes
            // as a last resort -- and cleaning that cache is routine maintenance.
            Self::Pacman => RollbackAbility::ManualFromCache,
            // dpkg has half-installed / half-configured states and a log, but no
            // undo.
            Self::Apt | Self::Apk => RollbackAbility::None,
        }
    }

    /// Whether this manager's project declares partial upgrades unsupported.
    pub const fn requires_full_system_upgrade(self) -> bool {
        matches!(self, Self::Pacman)
    }

    /// The read-only command that reports one package's installed version.
    ///
    /// Never elevates, and never relies on a default output format: where a
    /// format string is available it is given explicitly, so the parse target is
    /// osdk's choice rather than a default that may change between releases.
    pub fn query_command(self, package: &str) -> CommandSpec {
        match self {
            Self::Apt => CommandSpec::new("dpkg-query")
                .args(["-W", "-f=${Version}"])
                .arg(package),
            Self::Apk => CommandSpec::new("apk")
                .args(["info", "-e", "-v"])
                .arg(package),
            Self::Pacman => CommandSpec::new("pacman").arg("-Q").arg(package),
            Self::Dnf => CommandSpec::new("rpm")
                .args(["-q", "--qf", "%{VERSION}-%{RELEASE}"])
                .arg(package),
        }
    }

    /// The command a user would run to install a package themselves.
    ///
    /// On Arch this is `-Syu` with the package appended rather than `-S`, because
    /// Arch supports only full-system upgrades; printing `-S` would hand the user
    /// a partial upgrade their distribution says is unsupported.
    pub fn install_command(self, package: &str) -> CommandSpec {
        match self {
            Self::Apt => CommandSpec::new("sudo")
                .args(["apt-get", "install", "--only-upgrade=false", "-y"])
                .arg(package),
            Self::Apk => CommandSpec::new("sudo").args(["apk", "add"]).arg(package),
            Self::Pacman => CommandSpec::new("sudo")
                .args(["pacman", "-Syu", "--needed"])
                .arg(package),
            Self::Dnf => CommandSpec::new("sudo")
                .args(["dnf", "install", "-y"])
                .arg(package),
        }
    }
}

/// What a manager can undo after a transaction goes wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RollbackAbility {
    /// Atomic undo of a recorded transaction.
    Transactional,
    /// Only a manual downgrade from a cache that routine maintenance clears.
    ManualFromCache,
    /// A log, and nothing that undoes anything.
    None,
}

/// What osdk found about one Linux package manager.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DistroReport {
    pub manager: DistroManager,
    /// Whether the executable is present on this host.
    pub present: bool,
    pub rollback: RollbackAbility,
    /// Whether this manager's distribution supports only full-system upgrades.
    pub full_system_upgrade_only: bool,
}

/// Detect which Linux package managers this host has.
///
/// Runs one `--version` per candidate and nothing else: presence is all osdk
/// claims to know without asking about a specific package. On a non-Linux host
/// the list is empty rather than a set of absent entries, since "apt is missing
/// on Windows" is not a fact worth reporting.
pub fn detect(runner: &dyn CommandRunner, limits: CaptureLimits) -> Vec<DistroReport> {
    detect_for(runner, limits, cfg!(target_os = "linux"))
}

/// [`detect`] with the platform decision supplied.
///
/// Exists so the detection logic is testable everywhere rather than only on
/// Linux. A `cfg!` inside the function would make its interesting branch
/// unreachable on the machine most of this is developed on -- and an untested
/// branch in a subsystem whose whole job is reporting facts is exactly where a
/// wrong fact would survive.
pub fn detect_for(
    runner: &dyn CommandRunner,
    limits: CaptureLimits,
    is_linux: bool,
) -> Vec<DistroReport> {
    if !is_linux {
        return Vec::new();
    }
    DistroManager::ALL
        .into_iter()
        .map(|manager| {
            let outcome = runner.run_captured(&version_command(manager), limits);
            DistroReport {
                manager,
                present: matches!(
                    outcome,
                    CommandOutcome::Exited { ref status, .. } if status.success()
                ),
                rollback: manager.rollback(),
                full_system_upgrade_only: manager.requires_full_system_upgrade(),
            }
        })
        .collect()
}

fn version_command(manager: DistroManager) -> CommandSpec {
    CommandSpec::new(manager.program()).arg("--version")
}

/// Parse an installed version from a query command's stdout.
///
/// Each manager prints a different shape, and each is handled explicitly rather
/// than by a shared heuristic that would silently mis-parse one of them:
///
/// - `dpkg-query -W -f=${Version}` prints the bare version.
/// - `apk info -e -v` prints `name-version`.
/// - `pacman -Q` prints `name version`.
/// - `rpm -q --qf` prints the bare version.
///
/// Empty output yields `None`: a manager that printed nothing has not told us a
/// version, and inventing one would be worse than reporting the gap.
pub fn parse_installed_version(manager: DistroManager, stdout: &str) -> Option<String> {
    let text = stdout.trim();
    if text.is_empty() {
        return None;
    }
    match manager {
        DistroManager::Apt | DistroManager::Dnf => Some(text.to_owned()),
        // `pacman -Q git` prints "git 2.46.0-1".
        DistroManager::Pacman => text.split_whitespace().nth(1).map(str::to_owned),
        // `apk info -e -v git` prints "git-2.46.0-r0"; the version starts after
        // the last hyphen that precedes a digit, so rsplit on the package name
        // is unreliable. Take everything after the first hyphen followed by a
        // digit.
        DistroManager::Apk => apk_version(text),
    }
}

/// Split `name-1.2.3-r0` into its version part.
///
/// A package name may itself contain hyphens (`py3-foo-1.2-r0`), so the split
/// point is the first hyphen whose next character is a digit. Splitting on the
/// first hyphen outright would report `foo-1.2-r0` as the version.
fn apk_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for (index, window) in bytes.windows(2).enumerate() {
        if window[0] == b'-' && window[1].is_ascii_digit() {
            return Some(text[index + 1..].to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_command_is_read_only_and_never_elevates() {
        for manager in DistroManager::ALL {
            let command = manager.query_command("git");
            let program = command.program().to_string_lossy().into_owned();

            assert_ne!(
                program,
                "sudo",
                "{} must not elevate to answer a question",
                manager.id()
            );
            let args: Vec<String> = command
                .arguments()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(
                !args
                    .iter()
                    .any(|a| a == "install" || a == "add" || a == "-S"),
                "{} query must not be able to install: {args:?}",
                manager.id()
            );
        }
    }

    #[test]
    fn arch_gets_a_full_system_upgrade_rather_than_a_partial_one() {
        let command = DistroManager::Pacman.install_command("git");
        let args: Vec<String> = command
            .arguments()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(
            args.contains(&"-Syu".to_owned()),
            "Arch supports only full-system upgrades; -S alone is a partial upgrade, got {args:?}"
        );
        assert!(
            !args.contains(&"-Sy".to_owned()),
            "-Sy is what the Arch Wiki says never to run"
        );
        assert!(
            args.contains(&"--needed".to_owned()),
            "--needed keeps a rerun idempotent"
        );
    }

    #[test]
    fn only_arch_is_marked_as_requiring_a_full_system_upgrade() {
        assert!(DistroManager::Pacman.requires_full_system_upgrade());
        for manager in [DistroManager::Apt, DistroManager::Apk, DistroManager::Dnf] {
            assert!(
                !manager.requires_full_system_upgrade(),
                "{} does not declare partial upgrades unsupported",
                manager.id()
            );
        }
    }

    #[test]
    fn rollback_ability_is_reported_per_manager_rather_than_uniformly() {
        // The whole reason these are not driven: recovery differs structurally.
        assert_eq!(
            DistroManager::Dnf.rollback(),
            RollbackAbility::Transactional
        );
        assert_eq!(
            DistroManager::Pacman.rollback(),
            RollbackAbility::ManualFromCache
        );
        assert_eq!(DistroManager::Apt.rollback(), RollbackAbility::None);
    }

    #[test]
    fn zypper_is_absent_because_it_was_never_verified() {
        // Asserting it behaves like apt would state something unchecked.
        assert_eq!(DistroManager::ALL.len(), 4);
        assert!(!DistroManager::ALL.iter().any(|m| m.id() == "zypper"));
    }

    #[test]
    fn dpkg_output_is_the_bare_version() {
        assert_eq!(
            parse_installed_version(DistroManager::Apt, "2.46.0-1\n").as_deref(),
            Some("2.46.0-1")
        );
    }

    #[test]
    fn pacman_output_drops_the_package_name() {
        assert_eq!(
            parse_installed_version(DistroManager::Pacman, "git 2.46.0-1\n").as_deref(),
            Some("2.46.0-1")
        );
    }

    #[test]
    fn an_apk_package_name_containing_hyphens_keeps_its_whole_version() {
        // Splitting on the first hyphen would report "foo-1.2-r0" as the version.
        assert_eq!(
            parse_installed_version(DistroManager::Apk, "py3-foo-1.2-r0\n").as_deref(),
            Some("1.2-r0")
        );
        assert_eq!(
            parse_installed_version(DistroManager::Apk, "git-2.46.0-r0").as_deref(),
            Some("2.46.0-r0")
        );
    }

    #[test]
    fn rpm_output_is_the_bare_version() {
        assert_eq!(
            parse_installed_version(DistroManager::Dnf, "2.46.0-1.fc41\n").as_deref(),
            Some("2.46.0-1.fc41")
        );
    }

    #[test]
    fn empty_output_reports_no_version_rather_than_an_empty_one() {
        for manager in DistroManager::ALL {
            assert_eq!(parse_installed_version(manager, "   \n"), None);
            assert_eq!(parse_installed_version(manager, ""), None);
        }
    }

    #[test]
    fn output_without_a_version_yields_none_rather_than_the_name() {
        // `pacman -Q` with no version column, and an apk name with no digits.
        assert_eq!(parse_installed_version(DistroManager::Pacman, "git"), None);
        assert_eq!(
            parse_installed_version(DistroManager::Apk, "git-stable"),
            None
        );
    }

    /// A runner that reports a chosen set of managers as present.
    struct HostWith {
        present: Vec<&'static str>,
        probes: std::sync::Mutex<Vec<String>>,
    }

    impl CommandRunner for HostWith {
        fn run_captured(&self, command: &CommandSpec, _limits: CaptureLimits) -> CommandOutcome {
            let program = command.program().to_string_lossy().into_owned();
            self.probes.lock().unwrap().push(program.clone());

            // A query must never be able to change the system, so assert the
            // probe really is just a version check.
            let args: Vec<String> = command
                .arguments()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(args, vec!["--version".to_owned()], "detection probes only");

            if self.present.contains(&program.as_str()) {
                CommandOutcome::Exited {
                    status: probe_status(0),
                    output: crate::process::CapturedOutput::default(),
                }
            } else {
                CommandOutcome::NotInstalled
            }
        }

        fn run_foreground(
            &self,
            _command: &CommandSpec,
        ) -> std::io::Result<std::process::ExitStatus> {
            panic!("detection must never run a foreground command");
        }
    }

    #[cfg(windows)]
    fn probe_status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    #[cfg(unix)]
    fn probe_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    fn limits() -> CaptureLimits {
        CaptureLimits::new(std::time::Duration::from_secs(5), 1 << 16, 1 << 12)
    }

    #[test]
    fn a_non_linux_host_is_not_probed_at_all() {
        let host = HostWith {
            present: Vec::new(),
            probes: std::sync::Mutex::new(Vec::new()),
        };

        let reports = detect_for(&host, limits(), false);

        assert!(
            reports.is_empty(),
            "apt missing on Windows is not a finding"
        );
        assert!(
            host.probes.lock().unwrap().is_empty(),
            "nothing should be executed on a host that cannot have these managers"
        );
    }

    #[test]
    fn a_debian_host_reports_apt_present_and_the_others_absent() {
        let host = HostWith {
            present: vec!["apt-get"],
            probes: std::sync::Mutex::new(Vec::new()),
        };

        let reports = detect_for(&host, limits(), true);

        assert_eq!(reports.len(), 4, "every known manager appears either way");
        let apt = reports
            .iter()
            .find(|r| r.manager == DistroManager::Apt)
            .unwrap();
        assert!(apt.present);
        assert_eq!(apt.rollback, RollbackAbility::None);
        assert!(!apt.full_system_upgrade_only);

        assert!(
            reports
                .iter()
                .filter(|r| r.manager != DistroManager::Apt)
                .all(|r| !r.present),
            "a host with apt does not thereby have pacman"
        );
    }

    #[test]
    fn an_arch_host_is_flagged_as_full_system_upgrade_only() {
        let host = HostWith {
            present: vec!["pacman"],
            probes: std::sync::Mutex::new(Vec::new()),
        };

        let reports = detect_for(&host, limits(), true);
        let pacman = reports
            .iter()
            .find(|r| r.manager == DistroManager::Pacman)
            .unwrap();

        assert!(pacman.present);
        assert!(
            pacman.full_system_upgrade_only,
            "this is what tells a user not to let anything do partial upgrades here"
        );
        assert_eq!(pacman.rollback, RollbackAbility::ManualFromCache);
    }

    #[test]
    fn a_fedora_host_reports_the_one_manager_with_transactional_rollback() {
        let host = HostWith {
            present: vec!["dnf"],
            probes: std::sync::Mutex::new(Vec::new()),
        };

        let reports = detect_for(&host, limits(), true);
        let dnf = reports
            .iter()
            .find(|r| r.manager == DistroManager::Dnf)
            .unwrap();

        assert!(dnf.present);
        assert_eq!(dnf.rollback, RollbackAbility::Transactional);
    }

    #[test]
    fn each_manager_is_probed_exactly_once() {
        let host = HostWith {
            present: Vec::new(),
            probes: std::sync::Mutex::new(Vec::new()),
        };

        detect_for(&host, limits(), true);

        let probes = host.probes.lock().unwrap().clone();
        assert_eq!(probes.len(), 4, "one probe per manager, got {probes:?}");
        let mut unique = probes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 4, "no manager probed twice: {probes:?}");
    }
}
