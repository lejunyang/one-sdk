//! Installing the packages `[syspkg.packages]` asks for.
//!
//! # Why `--no-upgrade` is mandatory
//!
//! Verified on winget 1.29.290: `winget install` on a package that is already
//! present prints "found an existing installed package, attempting to upgrade"
//! and **starts downloading a newer version**. On a host holding Git 2.46.0 it
//! immediately began fetching 2.55.0.3.
//!
//! So "make sure this package exists" turns into "upgrade it while you are
//! there" -- an operation the user never asked for, which can undo a version
//! they deliberately stayed on. With `--no-upgrade` nothing is downloaded, the
//! version is untouched, and winget exits `0x8A150061`.
//!
//! That code therefore counts as **success**: the intent, that the package be
//! present, is satisfied. Treating non-zero as failure would make an idempotent
//! re-run report an error.

use serde::Serialize;

use super::apply::PlannedCommand;
use super::config::{PackageKey, PackageRequest};
use super::report::ManagerKind;
use super::status::{PackageState, PackageStatus};

/// Exit codes `winget install` returns, as measured on a real host.
pub mod install_exit_code {
    /// Installed successfully.
    pub const SUCCESS: i32 = 0;
    /// Already installed and `--no-upgrade` was passed. The intent is satisfied.
    pub const ALREADY_INSTALLED: i32 = -1978335135; // 0x8A150061
    /// No package matched the identifier.
    pub const NO_MATCH: i32 = -1978335212; // 0x8A150014
    /// The requested version does not exist for this package.
    pub const NO_SUCH_VERSION: i32 = -1978335209; // 0x8A150017
}

/// Whether an exit code means the package is now present.
///
/// A whitelist rather than `code == 0`, because "already installed" is a
/// non-zero code that nonetheless satisfies the request.
pub const fn install_succeeded(code: i32) -> bool {
    matches!(
        code,
        install_exit_code::SUCCESS | install_exit_code::ALREADY_INSTALLED
    )
}

/// Plain-language meaning of an install exit code.
///
/// Unknown codes are reported as the raw value rather than guessed at: winget
/// surfaces installer-defined codes too, and inventing an explanation for one
/// would mislead more than the number alone.
pub fn explain_install_code(code: i32) -> String {
    match code {
        install_exit_code::SUCCESS => "installed".to_owned(),
        install_exit_code::ALREADY_INSTALLED => "already installed".to_owned(),
        install_exit_code::NO_MATCH => {
            "no package matched this identifier -- check the id and the source".to_owned()
        }
        install_exit_code::NO_SUCH_VERSION => {
            "the requested version does not exist for this package".to_owned()
        }
        other => format!("winget exited {other} (0x{:X})", other as u32),
    }
}

/// One package an apply would install.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlannedInstall {
    pub manager: ManagerKind,
    pub id: String,
    /// Version requested, or `latest`.
    pub version: String,
    pub command: PlannedCommand,
}

/// What an apply would do, before it does it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallPlan {
    pub installs: Vec<PlannedInstall>,
    /// Packages needing nothing, with the reason.
    pub skipped: Vec<SkippedPackage>,
}

/// A package the plan deliberately leaves alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkippedPackage {
    pub id: String,
    pub reason: SkipReason,
}

/// Why a requested package is not part of the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// Present, and the request is satisfied.
    AlreadySatisfied,
    /// Present at another version.
    ///
    /// Deliberately not installed. The version in `[syspkg.packages]` is a wish
    /// for install time, not a lock, and reinstalling to force convergence would
    /// change a host the user did not ask to change.
    VersionDiffersButPresent,
    /// Not for this platform.
    NotApplicable,
    /// The manager could not be queried, so its packages cannot be planned.
    ManagerUnavailable,
    /// The manager is excluded by `[syspkg] managers`.
    ManagerNotAllowed,
}

impl InstallPlan {
    /// Whether this plan would change anything.
    pub fn is_empty(&self) -> bool {
        self.installs.is_empty()
    }
}

/// The command that installs one winget package.
///
/// Records argv rather than a command line so what runs is exactly what was
/// shown. Every flag here is deliberate:
///
/// - `--id` with `--exact` refuses a fuzzy match, which could install a
///   different package than the one written down.
/// - `--no-upgrade` keeps the operation to what was asked; see the module docs.
/// - The agreement flags are required for non-interactive use, and
///   `--disable-interactivity` guarantees it never waits for a prompt nobody is
///   there to answer.
/// - `--no-progress` is deliberately absent: it is undocumented on these
///   subcommands and buys nothing (V-8).
pub fn winget_install_command(id: &str, version: Option<&str>) -> PlannedCommand {
    let mut args = vec![
        "install".to_owned(),
        "--id".to_owned(),
        id.to_owned(),
        "--exact".to_owned(),
        "--no-upgrade".to_owned(),
    ];
    if let Some(version) = version {
        args.push("--version".to_owned());
        args.push(version.to_owned());
    }
    args.extend([
        "--accept-package-agreements".to_owned(),
        "--accept-source-agreements".to_owned(),
        "--disable-interactivity".to_owned(),
        "--nowarn".to_owned(),
    ]);
    PlannedCommand {
        program: "winget".to_owned(),
        args,
    }
}

/// Turn a status report into a plan.
///
/// Only `Missing` becomes an install. Everything else is skipped with a stated
/// reason, so the plan can be read as a complete account of every request rather
/// than a list whose omissions have to be inferred.
pub fn plan_installs(
    statuses: &[(PackageKey, PackageRequest, PackageStatus)],
    allowed: impl Fn(ManagerKind) -> bool,
) -> InstallPlan {
    let mut installs = Vec::new();
    let mut skipped = Vec::new();

    for (key, request, status) in statuses {
        if !allowed(key.manager) {
            skipped.push(SkippedPackage {
                id: key.id.clone(),
                reason: SkipReason::ManagerNotAllowed,
            });
            continue;
        }
        match status.state {
            PackageState::Missing => {
                let version = (!request.wants_latest()).then(|| request.version.clone());
                installs.push(PlannedInstall {
                    manager: key.manager,
                    id: key.id.clone(),
                    version: if request.wants_latest() {
                        "latest".to_owned()
                    } else {
                        request.version.clone()
                    },
                    command: winget_install_command(&key.id, version.as_deref()),
                });
            }
            PackageState::Satisfied => skipped.push(SkippedPackage {
                id: key.id.clone(),
                reason: SkipReason::AlreadySatisfied,
            }),
            PackageState::VersionDiffers => skipped.push(SkippedPackage {
                id: key.id.clone(),
                reason: SkipReason::VersionDiffersButPresent,
            }),
            PackageState::NotApplicable => skipped.push(SkippedPackage {
                id: key.id.clone(),
                reason: SkipReason::NotApplicable,
            }),
            PackageState::ManagerUnavailable => skipped.push(SkippedPackage {
                id: key.id.clone(),
                reason: SkipReason::ManagerUnavailable,
            }),
        }
    }

    InstallPlan { installs, skipped }
}

/// The result of installing one package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallResult {
    pub id: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub succeeded: bool,
    pub explanation: String,
}

/// Run an install plan, continuing past a package that fails.
///
/// One package failing does not abort the rest: they are independent requests,
/// and stopping would leave later packages uninstalled for a reason that has
/// nothing to do with them. Every outcome is returned so the caller can report
/// the whole picture and set an exit code from it.
pub fn run_installs(
    runner: &dyn crate::process::CommandRunner,
    plan: &InstallPlan,
) -> Vec<InstallResult> {
    plan.installs
        .iter()
        .map(|install| {
            let status = runner.run_foreground(&install.command.to_spec());
            let exit_code = status.as_ref().ok().and_then(|status| status.code());
            let succeeded = exit_code.is_some_and(install_succeeded);
            InstallResult {
                id: install.id.clone(),
                command: install.command.display(),
                exit_code,
                succeeded,
                explanation: match exit_code {
                    Some(code) => explain_install_code(code),
                    None => "winget could not be run".to_owned(),
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> PackageKey {
        PackageKey {
            manager: ManagerKind::Winget,
            id: id.to_owned(),
        }
    }

    fn request(version: &str) -> PackageRequest {
        PackageRequest {
            version: version.to_owned(),
            os: None,
        }
    }

    fn status(state: PackageState) -> PackageStatus {
        PackageStatus {
            manager: ManagerKind::Winget,
            id: "x".to_owned(),
            requested: "latest".to_owned(),
            installed: None,
            state,
        }
    }

    #[test]
    fn already_installed_counts_as_success_so_a_rerun_is_not_an_error() {
        // The whole point of --no-upgrade: this code means the intent is met.
        assert!(install_succeeded(install_exit_code::ALREADY_INSTALLED));
        assert!(install_succeeded(install_exit_code::SUCCESS));
    }

    #[test]
    fn a_missing_package_and_a_bad_version_are_both_failures() {
        assert!(!install_succeeded(install_exit_code::NO_MATCH));
        assert!(!install_succeeded(install_exit_code::NO_SUCH_VERSION));
    }

    #[test]
    fn an_unknown_code_is_reported_verbatim_rather_than_guessed() {
        let explanation = explain_install_code(1603);

        assert!(explanation.contains("1603"), "got: {explanation}");
        assert!(!install_succeeded(1603));
    }

    #[test]
    fn every_install_command_refuses_a_fuzzy_match_and_an_unrequested_upgrade() {
        let command = winget_install_command("Git.Git", None);
        let rendered = command.display();

        assert!(rendered.contains("--exact"), "got: {rendered}");
        assert!(
            rendered.contains("--no-upgrade"),
            "without this, installing an already-present package silently upgrades it"
        );
        assert!(rendered.contains("--disable-interactivity"));
    }

    #[test]
    fn the_undocumented_no_progress_flag_is_never_passed() {
        let rendered = winget_install_command("Git.Git", None).display();

        assert!(!rendered.contains("--no-progress"));
    }

    #[test]
    fn a_pinned_version_is_passed_through_verbatim() {
        let rendered = winget_install_command("Microsoft.PowerToys", Some("0.101.0")).display();

        assert!(rendered.contains("--version 0.101.0"), "got: {rendered}");
    }

    #[test]
    fn only_a_missing_package_becomes_an_install() {
        let statuses = vec![
            (key("A"), request("latest"), status(PackageState::Missing)),
            (key("B"), request("latest"), status(PackageState::Satisfied)),
        ];

        let plan = plan_installs(&statuses, |_| true);

        assert_eq!(plan.installs.len(), 1);
        assert_eq!(plan.installs[0].id, "A");
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, SkipReason::AlreadySatisfied);
    }

    #[test]
    fn a_present_package_at_another_version_is_left_alone() {
        let statuses = vec![(
            key("Git.Git"),
            request("2.99.0"),
            status(PackageState::VersionDiffers),
        )];

        let plan = plan_installs(&statuses, |_| true);

        assert!(
            plan.installs.is_empty(),
            "reinstalling to force a version would change a host nobody asked to change"
        );
        assert_eq!(plan.skipped[0].reason, SkipReason::VersionDiffersButPresent);
    }

    #[test]
    fn an_unavailable_manager_does_not_produce_installs() {
        let statuses = vec![(
            key("A"),
            request("latest"),
            status(PackageState::ManagerUnavailable),
        )];

        let plan = plan_installs(&statuses, |_| true);

        assert!(plan.installs.is_empty());
        assert_eq!(plan.skipped[0].reason, SkipReason::ManagerUnavailable);
    }

    #[test]
    fn a_manager_excluded_by_config_is_skipped_even_when_the_package_is_missing() {
        let statuses = vec![(key("A"), request("latest"), status(PackageState::Missing))];

        let plan = plan_installs(&statuses, |_| false);

        assert!(plan.installs.is_empty());
        assert_eq!(plan.skipped[0].reason, SkipReason::ManagerNotAllowed);
    }

    #[test]
    fn every_request_appears_somewhere_in_the_plan() {
        let statuses = vec![
            (key("A"), request("latest"), status(PackageState::Missing)),
            (key("B"), request("latest"), status(PackageState::Satisfied)),
            (
                key("C"),
                request("latest"),
                status(PackageState::NotApplicable),
            ),
        ];

        let plan = plan_installs(&statuses, |_| true);

        // A plan must account for all three, so its omissions never have to be
        // inferred.
        assert_eq!(plan.installs.len() + plan.skipped.len(), 3);
    }

    struct ScriptedRunner {
        codes: std::sync::Mutex<Vec<i32>>,
        calls: std::sync::Mutex<usize>,
    }

    impl crate::process::CommandRunner for ScriptedRunner {
        fn run_captured(
            &self,
            _command: &crate::process::CommandSpec,
            _limits: crate::process::CaptureLimits,
        ) -> crate::process::CommandOutcome {
            panic!("install must run in the foreground so winget's own output is visible");
        }

        fn run_foreground(
            &self,
            _command: &crate::process::CommandSpec,
        ) -> std::io::Result<std::process::ExitStatus> {
            *self.calls.lock().unwrap() += 1;
            let code = self.codes.lock().unwrap().remove(0);
            Ok(scripted_status(code))
        }
    }

    #[cfg(windows)]
    fn scripted_status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    #[cfg(unix)]
    fn scripted_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    fn plan_of(ids: &[&str]) -> InstallPlan {
        InstallPlan {
            installs: ids
                .iter()
                .map(|id| PlannedInstall {
                    manager: ManagerKind::Winget,
                    id: (*id).to_owned(),
                    version: "latest".to_owned(),
                    command: winget_install_command(id, None),
                })
                .collect(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn one_failing_package_does_not_abandon_the_others() {
        let runner = ScriptedRunner {
            codes: std::sync::Mutex::new(vec![
                install_exit_code::NO_MATCH,
                install_exit_code::SUCCESS,
            ]),
            calls: std::sync::Mutex::new(0),
        };

        let results = run_installs(&runner, &plan_of(&["Bad.Id", "Good.Id"]));

        assert_eq!(*runner.calls.lock().unwrap(), 2, "both were attempted");
        assert!(!results[0].succeeded);
        assert!(
            results[1].succeeded,
            "an unrelated failure must not block it"
        );
    }

    #[test]
    fn an_already_installed_package_is_reported_as_succeeding() {
        let runner = ScriptedRunner {
            codes: std::sync::Mutex::new(vec![install_exit_code::ALREADY_INSTALLED]),
            calls: std::sync::Mutex::new(0),
        };

        let results = run_installs(&runner, &plan_of(&["Git.Git"]));

        assert!(results[0].succeeded);
        assert_eq!(results[0].explanation, "already installed");
    }

    #[test]
    fn an_empty_plan_runs_nothing() {
        let runner = ScriptedRunner {
            codes: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::Mutex::new(0),
        };

        let results = run_installs(&runner, &plan_of(&[]));

        assert!(results.is_empty());
        assert_eq!(*runner.calls.lock().unwrap(), 0);
    }
}
