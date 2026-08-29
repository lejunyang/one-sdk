//! Native-runtime discovery and foreground delegation contracts.

use std::io;
use std::process::ExitStatus;

use super::redact::{CommandPurpose, NativeProgram, RedactedCommand};
use super::report::{DiagnosticReport, RuntimeKind};
use crate::process::{CaptureLimits, CommandOutcome, CommandRunner, CommandSpec};

/// An injectable native-runtime adapter. Implementations parse captured raw
/// output into typed report fields and must discard the raw bytes afterward.
pub trait RuntimeAdapter: Send + Sync {
    fn kind(&self) -> RuntimeKind;

    fn diagnose(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> DiagnosticReport;
}

/// A bounded read-only command used during native-runtime discovery.
///
/// This value is intentionally not serializable because its raw command can
/// contain endpoint selectors or other sensitive arguments. Use `evidence()`
/// when populating a diagnostic report.
#[derive(Debug)]
pub struct ProbeCommand {
    command: CommandSpec,
    evidence: RedactedCommand,
}

impl ProbeCommand {
    pub fn new(program: NativeProgram, purpose: CommandPurpose, command: CommandSpec) -> Self {
        let evidence = RedactedCommand::from_spec(program, purpose, &command);
        Self { command, evidence }
    }

    pub fn evidence(&self) -> &RedactedCommand {
        &self.evidence
    }

    pub fn execute(&self, runner: &dyn CommandRunner, limits: CaptureLimits) -> CommandOutcome {
        runner.run_captured(&self.command, limits)
    }
}

/// A single foreground native operation such as pull or prune.
///
/// `execute` consumes the value and delegates exactly once. It does not retry,
/// wrap the command in a shell, or capture any inherited stream.
#[derive(Debug)]
pub struct ForegroundCommand {
    command: CommandSpec,
    evidence: RedactedCommand,
}

impl ForegroundCommand {
    pub fn new(program: NativeProgram, purpose: CommandPurpose, command: CommandSpec) -> Self {
        let evidence = RedactedCommand::from_spec(program, purpose, &command);
        Self { command, evidence }
    }

    pub fn evidence(&self) -> &RedactedCommand {
        &self.evidence
    }

    pub fn execute(self, runner: &dyn CommandRunner) -> io::Result<ExitStatus> {
        runner.run_foreground(&self.command)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::process::CapturedOutput;

    #[derive(Default)]
    struct FakeRunner {
        captured_calls: AtomicUsize,
        foreground_calls: AtomicUsize,
    }

    impl CommandRunner for FakeRunner {
        fn run_captured(&self, _command: &CommandSpec, _limits: CaptureLimits) -> CommandOutcome {
            self.captured_calls.fetch_add(1, Ordering::Relaxed);
            CommandOutcome::TimedOut {
                output: CapturedOutput::default(),
                termination: crate::process::TerminationStatus::Requested,
            }
        }

        fn run_foreground(&self, _command: &CommandSpec) -> io::Result<ExitStatus> {
            self.foreground_calls.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::other("synthetic foreground failure"))
        }
    }

    #[test]
    fn probe_uses_injected_runner_and_exposes_only_redacted_evidence() {
        let runner = FakeRunner::default();
        let probe = ProbeCommand::new(
            NativeProgram::Docker,
            CommandPurpose::ContextInspect,
            CommandSpec::new("docker").args(["context", "inspect", "secret-context"]),
        );

        assert!(matches!(
            probe.execute(&runner, CaptureLimits::default()),
            CommandOutcome::TimedOut { .. }
        ));
        assert_eq!(runner.captured_calls.load(Ordering::Relaxed), 1);
        let json = serde_json::to_string(probe.evidence()).unwrap();
        assert!(!json.contains("secret-context"));
        assert!(json.contains("context-inspect"));
    }

    #[test]
    fn foreground_delegation_invokes_the_runner_exactly_once() {
        let runner = FakeRunner::default();
        let operation = ForegroundCommand::new(
            NativeProgram::Docker,
            CommandPurpose::Pull,
            CommandSpec::new("docker").args(["pull", "registry.example/secret"]),
        );

        assert!(operation.execute(&runner).is_err());
        assert_eq!(runner.foreground_calls.load(Ordering::Relaxed), 1);
    }
}
