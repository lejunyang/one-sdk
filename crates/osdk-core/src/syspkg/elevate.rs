//! Elevation policy for the Linux package managers.
//!
//! These managers require root, and how osdk gets there has to be decided
//! rather than assumed. The policy mirrors the one mise documents, because the
//! failure it avoids is specific and easy to hit: a tool that shells out to
//! `sudo` in a CI job with no TTY hangs forever waiting for a password nobody
//! will type. A build that hangs is worse than one that fails, since it burns
//! the whole job timeout before saying anything.
//!
//! Four cases, in the order they are checked:
//!
//! 1. **Already root** -- containers and CI. Run directly; invoking `sudo` there
//!    is pointless and may not even be installed.
//! 2. **Elevation forbidden by configuration** -- print the command instead.
//! 3. **Interactive terminal** -- `sudo` prompts as usual, which is what a user
//!    at a keyboard expects.
//! 4. **Non-interactive without passwordless sudo** -- refuse, and print the
//!    exact command. Never block on a prompt.
//!
//! Whichever case applies, the full command line is recorded before it runs.

use serde::Serialize;

use crate::process::CommandSpec;

/// How a privileged command should be run, or why it cannot be.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "decision", content = "detail")]
pub enum Elevation {
    /// Already root: run the command as it stands.
    AlreadyRoot,
    /// Prefix with `sudo`; it may prompt.
    Sudo,
    /// Prefix with `sudo --non-interactive`; it will not prompt.
    ///
    /// Used when passwordless sudo is available, so a CI job gets the benefit of
    /// elevation without the risk of a prompt.
    SudoNonInteractive,
    /// Do not elevate. The caller prints the command for the user to run.
    Refuse(RefusalReason),
}

/// Why osdk declined to elevate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalReason {
    /// `[syspkg] no_elevate = true`.
    ForbiddenByConfiguration,
    /// No TTY and no passwordless sudo, so a prompt would hang.
    WouldPromptWithoutATerminal,
    /// `sudo` is not installed.
    SudoUnavailable,
}

/// What osdk knows about the current process's ability to elevate.
///
/// Passed in rather than probed inside the decision so the policy is testable:
/// a rule about "no TTY in CI" that can only be exercised on a machine with no
/// TTY is a rule nobody will ever see fail.
#[derive(Clone, Copy, Debug)]
pub struct ElevationContext {
    pub is_root: bool,
    pub has_terminal: bool,
    pub sudo_present: bool,
    /// Whether `sudo -n true` succeeds, i.e. sudo needs no password.
    pub passwordless_sudo: bool,
    /// Whether configuration forbids elevation outright.
    pub elevation_forbidden: bool,
}

/// Decide how to run a command that needs root.
pub fn decide(context: ElevationContext) -> Elevation {
    // Root first: it makes every other question moot, and sudo may be absent in
    // a minimal container where it is also unnecessary.
    if context.is_root {
        return Elevation::AlreadyRoot;
    }
    if context.elevation_forbidden {
        return Elevation::Refuse(RefusalReason::ForbiddenByConfiguration);
    }
    if !context.sudo_present {
        return Elevation::Refuse(RefusalReason::SudoUnavailable);
    }
    // Passwordless sudo is safe even without a terminal, and is the common CI
    // setup, so it is checked before the terminal question rather than after.
    if context.passwordless_sudo {
        return Elevation::SudoNonInteractive;
    }
    if context.has_terminal {
        return Elevation::Sudo;
    }
    Elevation::Refuse(RefusalReason::WouldPromptWithoutATerminal)
}

impl Elevation {
    /// Whether osdk may run the command itself.
    pub const fn can_run(&self) -> bool {
        !matches!(self, Self::Refuse(_))
    }

    /// Apply this decision to a command.
    ///
    /// Returns `None` when osdk must not run it, so a caller cannot accidentally
    /// execute a refused command by ignoring a boolean.
    pub fn apply(&self, command: &CommandSpec) -> Option<CommandSpec> {
        let program = command.program().to_string_lossy().into_owned();
        let args: Vec<String> = command
            .arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        match self {
            Self::AlreadyRoot => Some(CommandSpec::new(&program).args(&args)),
            Self::Sudo => Some(CommandSpec::new("sudo").arg(&program).args(&args)),
            Self::SudoNonInteractive => Some(
                CommandSpec::new("sudo")
                    .arg("--non-interactive")
                    .arg(&program)
                    .args(&args),
            ),
            Self::Refuse(_) => None,
        }
    }

    /// How to explain a refusal, including what the user should do instead.
    pub const fn refusal_advice(reason: RefusalReason) -> &'static str {
        match reason {
            RefusalReason::ForbiddenByConfiguration => {
                "elevation is disabled by `[syspkg] no_elevate`; run this yourself"
            }
            RefusalReason::WouldPromptWithoutATerminal => {
                "sudo would prompt for a password and there is no terminal to type it into; \
                 run this yourself, or configure passwordless sudo"
            }
            RefusalReason::SudoUnavailable => "sudo is not installed; run this as root",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ElevationContext {
        ElevationContext {
            is_root: false,
            has_terminal: false,
            sudo_present: true,
            passwordless_sudo: false,
            elevation_forbidden: false,
        }
    }

    #[test]
    fn a_ci_job_without_a_terminal_is_refused_rather_than_left_hanging() {
        // The failure this whole module exists to prevent: a job that burns its
        // timeout waiting for a password nobody will type.
        let decision = decide(context());

        assert_eq!(
            decision,
            Elevation::Refuse(RefusalReason::WouldPromptWithoutATerminal)
        );
        assert!(!decision.can_run());
    }

    #[test]
    fn root_runs_directly_without_invoking_sudo() {
        let decision = decide(ElevationContext {
            is_root: true,
            // A minimal container may have no sudo at all, and needs none.
            sudo_present: false,
            ..context()
        });

        assert_eq!(decision, Elevation::AlreadyRoot);
        let command = decision
            .apply(&CommandSpec::new("apt-get").arg("install"))
            .expect("root may run it");
        assert_eq!(command.program().to_string_lossy(), "apt-get");
    }

    #[test]
    fn an_interactive_terminal_gets_an_ordinary_sudo_prompt() {
        let decision = decide(ElevationContext {
            has_terminal: true,
            ..context()
        });

        assert_eq!(decision, Elevation::Sudo);
        let command = decision
            .apply(&CommandSpec::new("apt-get").arg("install"))
            .unwrap();
        assert_eq!(command.program().to_string_lossy(), "sudo");
        let args: Vec<String> = command
            .arguments()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["apt-get", "install"]);
        assert!(
            !args.contains(&"--non-interactive".to_owned()),
            "a user at a keyboard should get the normal prompt"
        );
    }

    #[test]
    fn passwordless_sudo_is_used_even_without_a_terminal() {
        // The common CI setup: elevation works and cannot prompt, so refusing
        // would deny a capability that is actually available.
        let decision = decide(ElevationContext {
            passwordless_sudo: true,
            has_terminal: false,
            ..context()
        });

        assert_eq!(decision, Elevation::SudoNonInteractive);
        let command = decision.apply(&CommandSpec::new("apt-get")).unwrap();
        let args: Vec<String> = command
            .arguments()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.first().map(String::as_str),
            Some("--non-interactive"),
            "without this the sudo call could still block"
        );
    }

    #[test]
    fn configuration_can_forbid_elevation_even_where_it_would_work() {
        let decision = decide(ElevationContext {
            has_terminal: true,
            passwordless_sudo: true,
            elevation_forbidden: true,
            ..context()
        });

        assert_eq!(
            decision,
            Elevation::Refuse(RefusalReason::ForbiddenByConfiguration)
        );
        assert!(decision.apply(&CommandSpec::new("apt-get")).is_none());
    }

    #[test]
    fn being_root_overrides_a_configuration_that_forbids_elevation() {
        // `no_elevate` forbids *elevating*. It is not a request to refuse work
        // that needs no elevation at all.
        let decision = decide(ElevationContext {
            is_root: true,
            elevation_forbidden: true,
            ..context()
        });

        assert_eq!(decision, Elevation::AlreadyRoot);
    }

    #[test]
    fn a_missing_sudo_is_reported_as_its_own_reason() {
        let decision = decide(ElevationContext {
            sudo_present: false,
            has_terminal: true,
            ..context()
        });

        assert_eq!(decision, Elevation::Refuse(RefusalReason::SudoUnavailable));
        // The remedies differ, so the messages must too.
        assert_ne!(
            Elevation::refusal_advice(RefusalReason::SudoUnavailable),
            Elevation::refusal_advice(RefusalReason::WouldPromptWithoutATerminal)
        );
    }

    #[test]
    fn a_refused_decision_cannot_be_turned_into_a_command() {
        for reason in [
            RefusalReason::ForbiddenByConfiguration,
            RefusalReason::WouldPromptWithoutATerminal,
            RefusalReason::SudoUnavailable,
        ] {
            let refused = Elevation::Refuse(reason);
            assert!(
                refused.apply(&CommandSpec::new("apt-get")).is_none(),
                "a refusal must not be executable by ignoring a boolean"
            );
            assert!(!Elevation::refusal_advice(reason).is_empty());
        }
    }

    #[test]
    fn every_refusal_says_what_to_do_instead() {
        for reason in [
            RefusalReason::ForbiddenByConfiguration,
            RefusalReason::WouldPromptWithoutATerminal,
            RefusalReason::SudoUnavailable,
        ] {
            let advice = Elevation::refusal_advice(reason);
            assert!(
                advice.contains("run this") || advice.contains("configure"),
                "a refusal without a next step just blocks the user: {advice}"
            );
        }
    }
}
