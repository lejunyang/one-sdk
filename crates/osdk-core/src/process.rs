//! Helpers for running external commands (used by delegate backends like
//! rustup and corepack).
//!
//! [`CommandRunner`] is the injectable process boundary used by native
//! container-runtime adapters. Captured commands have explicit wall-clock and
//! byte limits. Foreground commands deliberately inherit all three standard
//! streams and are spawned exactly once without a shell.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// A command description that is intentionally not serializable.
///
/// Argument and environment values may contain credentials. Diagnostic output
/// must use the redacted evidence types in `container::redact`, never this raw
/// process description.
#[derive(Clone)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    env: BTreeMap<OsString, OsString>,
    cwd: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Add an environment override on top of the inherited environment.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Add environment overrides on top of the inherited environment.
    pub fn envs<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env.extend(
            env.into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        self
    }

    pub fn current_dir(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn program(&self) -> &OsStr {
        &self.program
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.args
    }

    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.env
    }

    pub fn working_directory(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).envs(&self.env);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command
    }
}

impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandSpec")
            .field("argument_count", &self.args.len())
            .field("environment_count", &self.env.len())
            .field("has_working_directory", &self.cwd.is_some())
            .finish()
    }
}

/// Hard limits for a captured child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureLimits {
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl CaptureLimits {
    pub const fn new(timeout: Duration, stdout_bytes: usize, stderr_bytes: usize) -> Self {
        Self {
            timeout,
            stdout_bytes,
            stderr_bytes,
        }
    }
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self::new(Duration::from_secs(10), 64 * 1024, 64 * 1024)
    }
}

/// Bounded raw output. This type deliberately does not implement `Serialize`;
/// native output can contain credentials and must be converted to typed,
/// redacted diagnostic evidence first.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CapturedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub elapsed: Duration,
}

impl std::fmt::Debug for CapturedOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CapturedOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("stdout_truncated", &self.stdout_truncated)
            .field("stderr_truncated", &self.stderr_truncated)
            .field("elapsed", &self.elapsed)
            .finish()
    }
}

/// Result of requesting termination after a capture deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminationStatus {
    Requested,
    AlreadyExited,
    Failed(io::ErrorKind),
}

/// Non-secret context for an otherwise ambiguous spawn error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnContext {
    /// The command supplied an explicit working directory. The operating
    /// system does not identify whether it or the executable caused errors
    /// such as `NotFound` or `PermissionDenied`.
    WithWorkingDirectory,
}

/// The explicit result of bounded captured execution.
#[derive(Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The executable could not be found.
    NotInstalled,
    /// The executable was found but the operating system denied execution.
    PermissionDenied,
    /// Spawning failed in a context where the error cannot safely be
    /// attributed to executable discovery, for example when a working
    /// directory was configured.
    SpawnFailed {
        kind: io::ErrorKind,
        context: SpawnContext,
    },
    /// The child exceeded its wall-clock limit. `termination` reports whether
    /// the operating system accepted the termination request; reaping proceeds
    /// asynchronously so the timeout remains a hard return bound.
    TimedOut {
        output: CapturedOutput,
        termination: TerminationStatus,
    },
    /// The child exited normally or by signal. Non-zero status is not an I/O
    /// error and is returned here unchanged.
    Exited {
        status: ExitStatus,
        output: CapturedOutput,
    },
    /// Spawning, polling, or setting up capture failed for another I/O reason.
    ExecutionFailed {
        kind: io::ErrorKind,
        output: CapturedOutput,
    },
}

impl std::fmt::Debug for CommandOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("NotInstalled"),
            Self::PermissionDenied => formatter.write_str("PermissionDenied"),
            Self::SpawnFailed { kind, context } => formatter
                .debug_struct("SpawnFailed")
                .field("kind", kind)
                .field("context", context)
                .finish(),
            Self::TimedOut {
                output,
                termination,
            } => formatter
                .debug_struct("TimedOut")
                .field("output", output)
                .field("termination", termination)
                .finish(),
            Self::Exited { status, output } => formatter
                .debug_struct("Exited")
                .field("status", status)
                .field("output", output)
                .finish(),
            Self::ExecutionFailed { kind, output } => formatter
                .debug_struct("ExecutionFailed")
                .field("kind", kind)
                .field("output", output)
                .finish(),
        }
    }
}

impl CommandOutcome {
    pub fn output(&self) -> Option<&CapturedOutput> {
        match self {
            Self::TimedOut { output, .. }
            | Self::Exited { output, .. }
            | Self::ExecutionFailed { output, .. } => Some(output),
            Self::NotInstalled | Self::PermissionDenied | Self::SpawnFailed { .. } => None,
        }
    }

    pub fn exit_status(&self) -> Option<ExitStatus> {
        match self {
            Self::Exited { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Injectable boundary for all native command execution.
pub trait CommandRunner: Send + Sync {
    /// Execute with null stdin, concurrently drained stdout/stderr, and hard
    /// time/byte ceilings.
    fn run_captured(&self, command: &CommandSpec, limits: CaptureLimits) -> CommandOutcome;

    /// Spawn one child directly, with inherited stdin/stdout/stderr, then
    /// return its native exit status. Implementations must not retry or invoke
    /// a shell.
    fn run_foreground(&self, command: &CommandSpec) -> io::Result<ExitStatus>;
}

/// The operating-system process runner.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run_captured(&self, command: &CommandSpec, limits: CaptureLimits) -> CommandOutcome {
        run_captured(command, limits)
    }

    fn run_foreground(&self, command: &CommandSpec) -> io::Result<ExitStatus> {
        let mut child = command.command();
        child
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        child.status()
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

enum StreamEvent {
    Data(Stream, Vec<u8>),
    Truncated(Stream),
    Done(Stream),
}

#[derive(Default)]
struct CaptureState {
    output: CapturedOutput,
    stdout_done: bool,
    stderr_done: bool,
}

impl CaptureState {
    fn apply(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Data(Stream::Stdout, bytes) => self.output.stdout.extend(bytes),
            StreamEvent::Data(Stream::Stderr, bytes) => self.output.stderr.extend(bytes),
            StreamEvent::Truncated(Stream::Stdout) => self.output.stdout_truncated = true,
            StreamEvent::Truncated(Stream::Stderr) => self.output.stderr_truncated = true,
            StreamEvent::Done(Stream::Stdout) => self.stdout_done = true,
            StreamEvent::Done(Stream::Stderr) => self.stderr_done = true,
        }
    }

    fn complete(&self) -> bool {
        self.stdout_done && self.stderr_done
    }
}

fn run_captured(command: &CommandSpec, limits: CaptureLimits) -> CommandOutcome {
    let started = Instant::now();
    let mut process = command.command();
    process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            if command.cwd.is_some() {
                return CommandOutcome::SpawnFailed {
                    kind: error.kind(),
                    context: SpawnContext::WithWorkingDirectory,
                };
            }
            return match error.kind() {
                io::ErrorKind::NotFound => CommandOutcome::NotInstalled,
                io::ErrorKind::PermissionDenied => CommandOutcome::PermissionDenied,
                kind => CommandOutcome::ExecutionFailed {
                    kind,
                    output: CapturedOutput {
                        elapsed: started.elapsed(),
                        ..CapturedOutput::default()
                    },
                },
            };
        }
    };

    let Some(stdout) = child.stdout.take() else {
        terminate_and_reap(child);
        return capture_failure(started, io::ErrorKind::Other);
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_and_reap(child);
        return capture_failure(started, io::ErrorKind::Other);
    };

    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    if thread::Builder::new()
        .name("osdk-stdout-drain".into())
        .spawn(move || drain_stream(stdout, Stream::Stdout, limits.stdout_bytes, stdout_sender))
        .is_err()
    {
        terminate_and_reap(child);
        return capture_failure(started, io::ErrorKind::Other);
    }
    if thread::Builder::new()
        .name("osdk-stderr-drain".into())
        .spawn(move || drain_stream(stderr, Stream::Stderr, limits.stderr_bytes, sender))
        .is_err()
    {
        terminate_and_reap(child);
        return capture_failure(started, io::ErrorKind::Other);
    }

    let mut capture = CaptureState::default();
    loop {
        receive_available(&receiver, &mut capture);
        match child.try_wait() {
            Ok(Some(status)) => {
                finish_capture(
                    &receiver,
                    &mut capture,
                    limits.timeout.saturating_sub(started.elapsed()),
                );
                capture.output.elapsed = started.elapsed();
                return CommandOutcome::Exited {
                    status,
                    output: capture.output,
                };
            }
            Ok(None) if started.elapsed() >= limits.timeout => {
                // Resolve the exit/timeout race once more before terminating.
                if let Ok(Some(status)) = child.try_wait() {
                    finish_capture(&receiver, &mut capture, Duration::ZERO);
                    capture.output.elapsed = started.elapsed();
                    return CommandOutcome::Exited {
                        status,
                        output: capture.output,
                    };
                }
                let termination = terminate_and_reap(child);
                finish_capture(&receiver, &mut capture, Duration::ZERO);
                capture.output.elapsed = started.elapsed();
                return CommandOutcome::TimedOut {
                    output: capture.output,
                    termination,
                };
            }
            Ok(None) => {
                let remaining = limits.timeout.saturating_sub(started.elapsed());
                thread::sleep(remaining.min(Duration::from_millis(2)));
            }
            Err(error) => {
                let kind = error.kind();
                terminate_and_reap(child);
                finish_capture(&receiver, &mut capture, Duration::ZERO);
                capture.output.elapsed = started.elapsed();
                return CommandOutcome::ExecutionFailed {
                    kind,
                    output: capture.output,
                };
            }
        }
    }
}

fn capture_failure(started: Instant, kind: io::ErrorKind) -> CommandOutcome {
    CommandOutcome::ExecutionFailed {
        kind,
        output: CapturedOutput {
            elapsed: started.elapsed(),
            ..CapturedOutput::default()
        },
    }
}

fn drain_stream(
    mut reader: impl Read,
    stream: Stream,
    limit: usize,
    sender: mpsc::Sender<StreamEvent>,
) {
    let mut retained = 0usize;
    let mut reported_truncation = false;
    let mut buffer = [0u8; 8 * 1024];

    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };

        let keep = count.min(limit.saturating_sub(retained));
        if keep > 0 {
            if sender
                .send(StreamEvent::Data(stream, buffer[..keep].to_vec()))
                .is_err()
            {
                return;
            }
            retained += keep;
        }
        if keep < count && !reported_truncation {
            if sender.send(StreamEvent::Truncated(stream)).is_err() {
                return;
            }
            reported_truncation = true;
        }
    }

    let _ = sender.send(StreamEvent::Done(stream));
}

fn receive_available(receiver: &Receiver<StreamEvent>, capture: &mut CaptureState) {
    loop {
        match receiver.try_recv() {
            Ok(event) => capture.apply(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

fn finish_capture(
    receiver: &Receiver<StreamEvent>,
    capture: &mut CaptureState,
    max_wait: Duration,
) {
    receive_available(receiver, capture);
    let started = Instant::now();
    while !capture.complete() {
        let remaining = max_wait.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok(event) => capture.apply(event),
            Err(_) => break,
        }
    }
    receive_available(receiver, capture);
}

fn terminate_and_reap(mut child: std::process::Child) -> TerminationStatus {
    let kill_error = child.kill().err().map(|error| error.kind());
    let (termination, reaped) = match child.try_wait() {
        Ok(Some(_)) => (TerminationStatus::AlreadyExited, true),
        Ok(None) => (
            kill_error
                .map(TerminationStatus::Failed)
                .unwrap_or(TerminationStatus::Requested),
            false,
        ),
        Err(error) => (
            TerminationStatus::Failed(kill_error.unwrap_or_else(|| error.kind())),
            false,
        ),
    };

    if !reaped {
        // Reaping cannot extend the caller's wall-clock limit. Once kill has
        // been requested, a detached waiter prevents a zombie without delaying
        // the diagnostic result.
        let _ = thread::Builder::new()
            .name("osdk-child-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
    termination
}

/// Run a command to completion, capturing stderr on failure. `env` overrides are
/// applied on top of the inherited environment.
pub fn run(
    program: &str,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = cmd.output().map_err(|e| Error::Command {
        cmd: format!("{program} {}", args.join(" ")),
        status: format!("failed to spawn: {e}"),
        stderr: None,
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Command {
            cmd: format!("{program} {}", args.join(" ")),
            status: output.status.to_string(),
            stderr: Some(String::from_utf8_lossy(&output.stderr).into_owned()),
        })
    }
}

pub fn output(
    program: &str,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> Result<std::process::Output> {
    let mut command = Command::new(program);
    command.args(args);
    command.envs(env);
    if let Some(directory) = cwd {
        command.current_dir(directory);
    }
    let output = command.output().map_err(|error| Error::Command {
        cmd: format!("{program} {}", args.join(" ")),
        status: format!("failed to spawn: {error}"),
        stderr: None,
    })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(Error::Command {
            cmd: format!("{program} {}", args.join(" ")),
            status: output.status.to_string(),
            stderr: Some(String::from_utf8_lossy(&output.stderr).into_owned()),
        })
    }
}

/// Whether `program` is resolvable on PATH.
pub fn exists(program: &str) -> bool {
    which::which(program).is_ok()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const CHILD_MODE: &str = "OSDK_PROCESS_TEST_CHILD_MODE";

    fn child_command(test_name: &str, mode: &str) -> CommandSpec {
        CommandSpec::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD_MODE, mode)
    }

    #[test]
    fn missing_program_is_classified_as_not_installed() {
        let command = CommandSpec::new(
            std::env::temp_dir().join("osdk-command-that-does-not-exist-4d912789"),
        );
        assert!(matches!(
            SystemCommandRunner.run_captured(&command, CaptureLimits::default()),
            CommandOutcome::NotInstalled
        ));
    }

    #[test]
    fn missing_working_directory_is_a_spawn_failure_not_not_installed() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("missing-working-directory");
        let command = CommandSpec::new(std::env::current_exe().unwrap()).current_dir(missing);

        assert!(matches!(
            SystemCommandRunner.run_captured(&command, CaptureLimits::default()),
            CommandOutcome::SpawnFailed {
                kind: io::ErrorKind::NotFound,
                context: SpawnContext::WithWorkingDirectory,
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_program_is_classified_as_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let program = temporary.path().join("not-executable");
        std::fs::write(&program, b"not executable").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(
            SystemCommandRunner.run_captured(&CommandSpec::new(program), CaptureLimits::default()),
            CommandOutcome::PermissionDenied
        ));
    }

    #[test]
    fn captured_output_is_bounded_while_both_pipes_are_drained() {
        let outcome = SystemCommandRunner.run_captured(
            &child_command("process::tests::child_emits_large_output", "large-output"),
            CaptureLimits::new(Duration::from_secs(10), 113, 97),
        );
        let CommandOutcome::Exited { status, output } = outcome else {
            panic!("expected exited child, got {outcome:?}");
        };

        assert!(status.success());
        assert_eq!(output.stdout.len(), 113);
        assert_eq!(output.stderr.len(), 97);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[test]
    fn captured_process_has_a_wall_clock_timeout() {
        let outcome = SystemCommandRunner.run_captured(
            &child_command("process::tests::child_blocks", "block"),
            CaptureLimits::new(Duration::from_millis(100), 1024, 1024),
        );
        let CommandOutcome::TimedOut { output, .. } = outcome else {
            panic!("expected timed-out child, got {outcome:?}");
        };

        assert!(output.elapsed >= Duration::from_millis(50));
        assert!(output.elapsed < Duration::from_millis(750));
    }

    #[test]
    fn debug_output_does_not_expose_captured_bytes_or_command_details() {
        let secret = "never-print-this-secret";
        let command = CommandSpec::new(secret)
            .arg(secret)
            .env(secret, secret)
            .current_dir(secret);
        let output = CapturedOutput {
            stdout: secret.as_bytes().to_vec(),
            stderr: secret.as_bytes().to_vec(),
            ..CapturedOutput::default()
        };
        let outcome = CommandOutcome::ExecutionFailed {
            kind: io::ErrorKind::Other,
            output,
        };

        assert!(!format!("{command:?}").contains(secret));
        assert!(!format!("{outcome:?}").contains(secret));
    }

    #[test]
    fn child_emits_large_output() {
        if std::env::var_os(CHILD_MODE).as_deref() != Some(OsStr::new("large-output")) {
            return;
        }

        std::io::stdout()
            .write_all(&vec![b'o'; 128 * 1024])
            .unwrap();
        std::io::stderr()
            .write_all(&vec![b'e'; 128 * 1024])
            .unwrap();
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
    }

    #[test]
    fn child_blocks() {
        if std::env::var_os(CHILD_MODE).as_deref() != Some(OsStr::new("block")) {
            return;
        }

        thread::sleep(Duration::from_secs(5));
    }
}
