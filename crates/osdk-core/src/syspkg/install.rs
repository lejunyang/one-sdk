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
use super::distro::DistroManager;
use super::distro_mirror::EphemeralAptSource;
use super::elevate::Elevation;
use super::report::ManagerKind;
use super::status::{PackageState, PackageStatus};
use crate::process::CommandSpec;

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

/// Whether an exit code means the package is now present, for any manager.
///
/// winget's whitelist must not be applied to a distro manager: `-1978335135`
/// means "already installed" to winget and nothing at all to apt, while apt
/// signals the same situation with a plain 0. Sharing one table would make the
/// rule right by accident for the common case and wrong for every other code.
pub const fn install_succeeded_for(manager: ManagerKind, code: i32) -> bool {
    match manager {
        ManagerKind::Winget => install_succeeded(code),
        // apt-get, apk and dnf all exit 0 when the package ends up installed,
        // including when it already was. There is no second success code to
        // whitelist, so anything non-zero is a real failure.
        ManagerKind::Distro(_) => code == 0,
        ManagerKind::Homebrew => code == 0,
    }
}

/// Plain-language meaning of an exit code, for any manager.
pub fn explain_install_code_for(manager: ManagerKind, code: i32) -> String {
    match manager {
        ManagerKind::Winget => explain_install_code(code),
        ManagerKind::Distro(distro) => match code {
            0 => "installed".to_owned(),
            // Documented by apt: 100 is the code for a package that cannot be
            // found or a dependency problem, which is by far the most common
            // failure and worth naming rather than printing bare.
            100 => format!(
                "{} exited 100 -- the package was not found, or its dependencies \
                 could not be satisfied",
                distro.id()
            ),
            other => format!("{} exited {other}", distro.id()),
        },
        ManagerKind::Homebrew => match code {
            0 => "installed".to_owned(),
            other => format!("brew exited {other}"),
        },
    }
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
    /// Installing a single package on Arch is a partial upgrade.
    ///
    /// Arch documents that partial upgrades are unsupported -- never `pacman
    /// -Sy`, always `pacman -Syu` -- and asks that the full upgrade be run by
    /// the person at the keyboard, after reading the news. Installing one
    /// package is exactly the operation upstream declines to support, so osdk
    /// prints the command instead of running it.
    PacmanWantsAFullUpgrade,
}

impl InstallPlan {
    /// Whether this plan would change anything.
    pub fn is_empty(&self) -> bool {
        self.installs.is_empty()
    }

    /// Whether any command in this plan needs root.
    ///
    /// Lets the caller report a refusal once, up front, instead of repeating it
    /// per package -- and stay quiet when the plan is winget-only, where the
    /// question never arises.
    pub fn needs_elevation(&self) -> bool {
        self.installs
            .iter()
            .any(|install| matches!(install.manager, ManagerKind::Distro(_)))
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

/// The command that installs one package, whichever manager owns it.
///
/// Returns `None` when the package must not be installed by osdk at all. That
/// is not the same as "no command exists": for pacman a perfectly good command
/// exists, and upstream asks that a human run it.
pub fn install_command_for(
    manager: ManagerKind,
    id: &str,
    version: Option<&str>,
) -> Option<PlannedCommand> {
    match manager {
        ManagerKind::Winget => Some(winget_install_command(id, version)),
        ManagerKind::Distro(DistroManager::Pacman) => None,
        ManagerKind::Distro(distro) => Some(distro_install_command(distro, id, version)),
        // Homebrew's install path is not implemented; planning a command for it
        // would claim a capability that does not exist.
        ManagerKind::Homebrew => None,
    }
}

/// The command that installs one distro package.
///
/// The argv comes from `DistroManager::install_command`, which is also what
/// `pkg doctor` prints, so the command shown and the command run cannot drift
/// apart. Elevation is applied later, by the caller: a plan is a description,
/// and prefixing `sudo` here would bake in a decision that depends on the
/// machine the plan finally runs on.
fn distro_install_command(
    manager: DistroManager,
    id: &str,
    version: Option<&str>,
) -> PlannedCommand {
    // A version is honoured only where the manager has a documented syntax for
    // it. apt and dnf take `name=version` and `name-version`; apk takes
    // `name=version`. Where the spelling is not certain the request degrades to
    // the plain name rather than guessing at syntax that might select the wrong
    // package or fail obscurely.
    let spec = match (manager, version) {
        (DistroManager::Apt, Some(version)) => format!("{id}={version}"),
        (DistroManager::Apk, Some(version)) => format!("{id}={version}"),
        (DistroManager::Dnf, Some(version)) => format!("{id}-{version}"),
        _ => id.to_owned(),
    };

    let command = manager.install_command(&spec);
    PlannedCommand {
        program: command.program().to_string_lossy().into_owned(),
        args: command
            .arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect(),
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
                match install_command_for(key.manager, &key.id, version.as_deref()) {
                    Some(command) => installs.push(PlannedInstall {
                        manager: key.manager,
                        id: key.id.clone(),
                        version: if request.wants_latest() {
                            "latest".to_owned()
                        } else {
                            request.version.clone()
                        },
                        command,
                    }),
                    // Absent by policy, not by oversight, so it is reported as a
                    // skip with a reason rather than silently dropped.
                    None => skipped.push(SkippedPackage {
                        id: key.id.clone(),
                        reason: match key.manager {
                            ManagerKind::Distro(DistroManager::Pacman) => {
                                SkipReason::PacmanWantsAFullUpgrade
                            }
                            _ => SkipReason::ManagerNotAllowed,
                        },
                    }),
                }
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

/// Insert options immediately after the program, before its subcommand.
///
/// apt only honours `-o` before the subcommand; later they become package
/// names. Rebuilding the spec is the clearest way to guarantee the position,
/// since a CommandSpec has no "insert at index" operation.
fn prepend_options(spec: &crate::process::CommandSpec, options: &[String]) -> CommandSpec {
    let program = spec.program().to_string_lossy().into_owned();
    let existing: Vec<String> = spec
        .arguments()
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();

    CommandSpec::new(&program).args(options).args(&existing)
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
///
/// `elevation` is the decision made once for this host and applied to every
/// distro command. A refusal is not an error: the command is recorded as it
/// would have been run, marked unsuccessful, and explained -- which is what lets
/// the caller print something the user can paste into a root shell. winget
/// commands never take it; UAC is the installer's business, not a prefix.
pub fn run_installs(
    runner: &dyn crate::process::CommandRunner,
    plan: &InstallPlan,
    elevation: &Elevation,
    acceleration: Option<&EphemeralAptSource>,
) -> Vec<InstallResult> {
    plan.installs
        .iter()
        .map(|install| {
            let spec = install.command.to_spec();

            // Mirror options go on before anything else, because apt requires
            // its `-o` flags ahead of the subcommand. Appending them after
            // `install` makes apt treat them as package names -- a failure that
            // reads as "package -oDir::Cache=... not found" and sends the reader
            // looking in the wrong place entirely.
            let spec = match (install.manager, acceleration) {
                (ManagerKind::Distro(DistroManager::Apt), Some(source)) => {
                    prepend_options(&spec, &source.options)
                }
                _ => spec,
            };

            // Only the distro managers need root. Deciding this per package
            // rather than per run keeps a mixed plan honest on a host where one
            // manager needs elevation and the other does not.
            let (spec, refusal) = match install.manager {
                ManagerKind::Distro(_) => match elevation.apply(&spec) {
                    Some(elevated) => (Some(elevated), None),
                    None => (None, elevation.refusal()),
                },
                _ => (Some(spec), None),
            };

            let Some(spec) = spec else {
                let reason = refusal.expect("a refused command always carries a reason");
                return InstallResult {
                    id: install.id.clone(),
                    command: install.command.display(),
                    exit_code: None,
                    succeeded: false,
                    explanation: format!("not run: {}", Elevation::refusal_advice(reason)),
                };
            };

            // Environment the manager needs to stay non-interactive. Applied
            // here rather than baked into the plan so what is displayed stays a
            // plain command line.
            let spec = match install.manager {
                ManagerKind::Distro(distro) => distro
                    .noninteractive_env()
                    .into_iter()
                    .fold(spec, |spec, (key, value)| spec.env(key, value)),
                _ => spec,
            };

            let status = runner.run_foreground(&spec);
            let exit_code = status.as_ref().ok().and_then(|status| status.code());
            let succeeded =
                exit_code.is_some_and(|code| install_succeeded_for(install.manager, code));
            InstallResult {
                id: install.id.clone(),
                command: install.command.display(),
                exit_code,
                succeeded,
                explanation: match exit_code {
                    Some(code) => explain_install_code_for(install.manager, code),
                    None => format!("{} could not be run", install.command.program),
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syspkg::elevate::RefusalReason;

    fn key(id: &str) -> PackageKey {
        PackageKey {
            manager: ManagerKind::Winget,
            id: id.to_owned(),
        }
    }

    fn request(version: &str) -> PackageRequest {
        PackageRequest {
            version: version.to_owned(),
            platform: Default::default(),
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

    /// A Unix wait status carries only an 8-bit exit code, so this round-trips
    /// small codes and truncates large ones: winget's `ALREADY_INSTALLED`
    /// (-1978335135) comes back from `.code()` as 97. That is a limit of the
    /// platform's process API, not something to encode around -- tests whose
    /// input is a full 32-bit winget code are gated to Windows instead.
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

        let results = run_installs(
            &runner,
            &plan_of(&["Bad.Id", "Good.Id"]),
            &Elevation::AlreadyRoot,
            None,
        );

        assert_eq!(*runner.calls.lock().unwrap(), 2, "both were attempted");
        assert!(!results[0].succeeded);
        assert!(
            results[1].succeeded,
            "an unrelated failure must not block it"
        );
    }

    /// Windows-only because the input is winget's 32-bit `ALREADY_INSTALLED`
    /// code, which a Unix wait status cannot represent (see `scripted_status`).
    /// The judgement itself is covered on every platform by
    /// `already_installed_counts_as_success_so_a_rerun_is_not_an_error` and
    /// `a_distro_exit_code_is_not_judged_by_wingets_table`, which call
    /// `install_succeeded*` directly; what this adds is the path through
    /// `run_installs` and `ExitStatus`.
    #[cfg(windows)]
    #[test]
    fn an_already_installed_package_is_reported_as_succeeding() {
        let runner = ScriptedRunner {
            codes: std::sync::Mutex::new(vec![install_exit_code::ALREADY_INSTALLED]),
            calls: std::sync::Mutex::new(0),
        };

        let results = run_installs(
            &runner,
            &plan_of(&["Git.Git"]),
            &Elevation::AlreadyRoot,
            None,
        );

        assert!(results[0].succeeded);
        assert_eq!(results[0].explanation, "already installed");
    }

    #[test]
    fn an_empty_plan_runs_nothing() {
        let runner = ScriptedRunner {
            codes: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::Mutex::new(0),
        };

        let results = run_installs(&runner, &plan_of(&[]), &Elevation::AlreadyRoot, None);

        assert!(results.is_empty());
        assert_eq!(*runner.calls.lock().unwrap(), 0);
    }

    /// Records what was actually launched, which is the only way to tell an
    /// elevated command from an unelevated one: both succeed.
    #[derive(Default)]
    struct RecordingRunner {
        commands: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl crate::process::CommandRunner for RecordingRunner {
        fn run_captured(
            &self,
            _command: &crate::process::CommandSpec,
            _limits: crate::process::CaptureLimits,
        ) -> crate::process::CommandOutcome {
            panic!("an install runs in the foreground");
        }

        fn run_foreground(
            &self,
            command: &crate::process::CommandSpec,
        ) -> std::io::Result<std::process::ExitStatus> {
            self.commands.lock().unwrap().push((
                command.program().to_string_lossy().into_owned(),
                command
                    .arguments()
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
            ));
            Ok(scripted_status(0))
        }
    }

    fn distro_key(manager: DistroManager, id: &str) -> PackageKey {
        PackageKey {
            manager: ManagerKind::Distro(manager),
            id: id.to_owned(),
        }
    }

    fn missing(id: &str) -> PackageStatus {
        PackageStatus {
            manager: ManagerKind::Winget,
            id: id.to_owned(),
            requested: "latest".to_owned(),
            installed: None,
            state: PackageState::Missing,
        }
    }

    #[test]
    fn a_distro_package_is_planned_with_its_own_managers_command() {
        // The bug this guards: plan_installs used to build a winget command for
        // every missing package, so an apt entry became `winget install libfoo`.
        let statuses = vec![(
            distro_key(DistroManager::Apt, "libssl-dev"),
            request("latest"),
            missing("libssl-dev"),
        )];

        let plan = plan_installs(&statuses, |_| true);

        assert_eq!(plan.installs.len(), 1);
        let command = &plan.installs[0].command;
        assert_eq!(command.program, "apt-get");
        assert!(
            command.args.contains(&"install".to_owned())
                && command.args.contains(&"libssl-dev".to_owned()),
            "unexpected argv: {:?}",
            command.args
        );
        assert!(
            !command.args.iter().any(|a| a.contains("winget")),
            "a distro package must not be planned through winget"
        );
    }

    #[test]
    fn every_distro_manager_plans_through_its_own_program() {
        let cases = [
            (DistroManager::Apt, "apt-get"),
            (DistroManager::Apk, "apk"),
            (DistroManager::Dnf, "dnf"),
        ];

        for (manager, program) in cases {
            let command = install_command_for(ManagerKind::Distro(manager), "pkg", None)
                .expect("this manager installs");
            assert_eq!(
                command.program, program,
                "{manager:?} used the wrong program"
            );
        }
    }

    #[test]
    fn pacman_is_planned_as_advice_rather_than_an_install() {
        // Arch documents that installing one package is a partial upgrade and
        // unsupported. Skipping it must be visible, not silent.
        let statuses = vec![(
            distro_key(DistroManager::Pacman, "base-devel"),
            request("latest"),
            missing("base-devel"),
        )];

        let plan = plan_installs(&statuses, |_| true);

        assert!(plan.installs.is_empty(), "pacman must not be run by osdk");
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, SkipReason::PacmanWantsAFullUpgrade);
        assert!(
            install_command_for(ManagerKind::Distro(DistroManager::Pacman), "x", None).is_none()
        );
    }

    #[test]
    fn a_pinned_version_uses_each_managers_own_syntax() {
        let apt = install_command_for(ManagerKind::Distro(DistroManager::Apt), "git", Some("1.2"))
            .expect("apt installs");
        assert!(apt.args.contains(&"git=1.2".to_owned()), "{:?}", apt.args);

        let dnf = install_command_for(ManagerKind::Distro(DistroManager::Dnf), "git", Some("1.2"))
            .expect("dnf installs");
        assert!(dnf.args.contains(&"git-1.2".to_owned()), "{:?}", dnf.args);
    }

    #[test]
    fn elevation_is_applied_to_distro_commands_only() {
        let statuses = vec![
            (
                distro_key(DistroManager::Apt, "libfoo"),
                request("latest"),
                missing("libfoo"),
            ),
            (
                key("Some.Winget"),
                request("latest"),
                missing("Some.Winget"),
            ),
        ];
        let plan = plan_installs(&statuses, |_| true);
        let runner = RecordingRunner::default();

        let _ = run_installs(&runner, &plan, &Elevation::Sudo, None);

        let ran = runner.commands.lock().unwrap();
        assert_eq!(ran.len(), 2);
        assert_eq!(ran[0].0, "sudo", "apt must be elevated");
        assert_eq!(
            ran[1].0, "winget",
            "winget handles UAC itself and must not be prefixed"
        );
    }

    #[test]
    fn a_refused_elevation_runs_nothing_and_says_why() {
        let statuses = vec![(
            distro_key(DistroManager::Apt, "libfoo"),
            request("latest"),
            missing("libfoo"),
        )];
        let plan = plan_installs(&statuses, |_| true);
        let runner = RecordingRunner::default();

        let results = run_installs(
            &runner,
            &plan,
            &Elevation::Refuse(RefusalReason::WouldPromptWithoutATerminal),
            None,
        );

        assert!(
            runner.commands.lock().unwrap().is_empty(),
            "a refusal must not run the command anyway"
        );
        assert!(!results[0].succeeded);
        assert!(
            results[0].explanation.starts_with("not run:"),
            "explanation was {:?}",
            results[0].explanation
        );
        // The command is still reported, so it can be copied into a root shell.
        assert!(results[0].command.contains("apt-get"));
    }

    #[test]
    fn a_distro_exit_code_is_not_judged_by_wingets_table() {
        // -1978335135 is winget's "already installed". For apt it is just a
        // failure, and reading it as success would report a package as present
        // when it is not.
        assert!(install_succeeded_for(
            ManagerKind::Winget,
            install_exit_code::ALREADY_INSTALLED
        ));
        assert!(!install_succeeded_for(
            ManagerKind::Distro(DistroManager::Apt),
            install_exit_code::ALREADY_INSTALLED
        ));
        assert!(install_succeeded_for(
            ManagerKind::Distro(DistroManager::Apt),
            0
        ));
    }

    #[test]
    fn a_distro_failure_is_explained_without_naming_winget() {
        let text = explain_install_code_for(ManagerKind::Distro(DistroManager::Apt), 100);
        assert!(text.contains("apt"), "explanation was {text:?}");
        assert!(
            !text.contains("winget"),
            "an apt failure must not be attributed to winget: {text:?}"
        );
    }

    #[test]
    fn only_a_plan_with_distro_packages_needs_elevation() {
        let winget_only =
            plan_installs(&[(key("A.B"), request("latest"), missing("A.B"))], |_| true);
        assert!(!winget_only.needs_elevation());

        let with_apt = plan_installs(
            &[(
                distro_key(DistroManager::Apt, "libfoo"),
                request("latest"),
                missing("libfoo"),
            )],
            |_| true,
        );
        assert!(with_apt.needs_elevation());
    }
}
