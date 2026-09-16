//! winget discovery.
//!
//! Everything here is read-only: osdk asks winget what it is and what sources
//! it has, and installs nothing.
//!
//! Two measured facts from a real Windows 11 host running winget v1.29.290
//! shape this module, and both are easy to get wrong from the documentation
//! alone:
//!
//! 1. **The human-readable output is localised.** `winget list` prints its
//!    columns as 名称 / ID / 版本 / 可用 / 源 on a Chinese host. A parser
//!    written against English headers does not merely fail there, it fails
//!    *silently* -- a column it cannot find reads as an absent package. So this
//!    module never parses those tables. `winget source export` emits JSON Lines
//!    whose keys stay English, and that is the path taken.
//!
//! 2. **Exit codes are the one locale-independent channel.** A missing package
//!    is 0x8A150014 whatever the display language, so classification keys off
//!    the code and never off the message.

use std::collections::BTreeSet;

use serde::Deserialize;

use super::report::{
    Capability, CapabilityStatus, ManagerDetails, ManagerKind, ManagerReport, ManagerStatus,
    ProbeOutcome, ProbePurpose, ProbeRecord, SourceRecord, SourceTrust, WingetDetails,
};
use crate::process::{CaptureLimits, CommandOutcome, CommandRunner, CommandSpec};

/// winget HRESULTs osdk reasons about.
///
/// Values are the raw `i32` exit codes as observed, so they can be compared
/// against `ExitStatus::code()` directly.
pub mod exit_code {
    /// `APPINSTALLER_CLI_ERROR_NO_APPLICATIONS_FOUND` (0x8A150014): the query
    /// matched nothing. Verified on 1.29.290 for both `list` and `show`.
    pub const NO_APPLICATIONS_FOUND: i32 = -1978335212;
    /// `APPINSTALLER_CLI_ERROR_SOURCE_NAME_DOES_NOT_EXIST` (0x8A150012).
    pub const SOURCE_NAME_DOES_NOT_EXIST: i32 = -1978335214;
    /// `APPINSTALLER_CLI_ERROR_INVALID_CL_ARGUMENTS` (0x8A150002): winget did
    /// not recognise a flag. For osdk this means a bug in its own call, not a
    /// problem with the host.
    pub const INVALID_CL_ARGUMENTS: i32 = -1978335230;
}

/// Default source identifiers shipped by winget itself.
///
/// Used to tell a stock host from one pointed at a mirror. Matching is on the
/// identifier rather than the display name because the name is user-assignable.
const BUILTIN_SOURCE_IDENTIFIERS: &[&str] = &[
    "Microsoft.Winget.Source_8wekyb3d8bbwe",
    "Microsoft.Winget.Fonts.Source_8wekyb3d8bbwe",
    "StoreEdgeFD",
];

/// One line of `winget source export` output.
///
/// Deserialization is deliberately lenient: unknown fields are ignored so a
/// future winget can add keys without breaking discovery, and every field is
/// optional because osdk must not reject a source just because one attribute is
/// missing.
#[derive(Debug, Deserialize)]
struct ExportedSource {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Identifier")]
    identifier: Option<String>,
    #[serde(rename = "Arg")]
    arg: Option<String>,
    #[serde(rename = "Type")]
    kind: Option<String>,
    #[serde(rename = "TrustLevel", default)]
    trust_level: Vec<String>,
}

impl ExportedSource {
    fn into_record(self) -> Option<SourceRecord> {
        // A source osdk cannot name is a source it cannot report on.
        let identifier = self.identifier.or_else(|| self.name.clone())?;
        let name = self.name.unwrap_or_else(|| identifier.clone());
        let trust = if self
            .trust_level
            .iter()
            .any(|level| level.eq_ignore_ascii_case("trusted"))
        {
            SourceTrust::Trusted
        } else if self.trust_level.is_empty() {
            SourceTrust::Unknown
        } else {
            SourceTrust::Untrusted
        };

        Some(SourceRecord {
            identifier,
            name,
            endpoint: self.arg,
            kind: self.kind,
            trust,
        })
    }
}

/// Parse the JSON Lines emitted by `winget source export`.
///
/// Each line is an independent object. A malformed line is skipped rather than
/// failing the pass, so one unparseable source does not hide the others.
pub fn parse_exported_sources(stdout: &str) -> Vec<SourceRecord> {
    let mut records: Vec<SourceRecord> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<ExportedSource>(line).ok())
        .filter_map(ExportedSource::into_record)
        .collect();

    // Deterministic output regardless of the order winget listed them in.
    records.sort();
    records.dedup();
    records
}

/// Extract the client version from `winget --version`.
///
/// The command prints exactly the version (`v1.29.290`) and nothing else, so
/// this takes the first non-empty line rather than searching for a label --
/// a label would be localised.
pub fn parse_version(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

/// Whether any configured source is not one winget ships with.
pub fn has_non_default_source(sources: &[SourceRecord]) -> bool {
    sources
        .iter()
        .any(|source| !BUILTIN_SOURCE_IDENTIFIERS.contains(&source.identifier.as_str()))
}

/// The flags osdk attaches to every winget invocation.
///
/// `--no-progress` is deliberately absent. It was measured to make no
/// difference once stdout is redirected (identical bytes, no escape sequences
/// either way) and it no longer appears in the help text of any subcommand
/// osdk calls, though it is still accepted. Depending on an undocumented flag
/// that buys nothing is a liability, so the convention uses two options that
/// are currently documented.
fn common_flags() -> [&'static str; 2] {
    ["--disable-interactivity", "--nowarn"]
}

fn version_command() -> CommandSpec {
    CommandSpec::new(ManagerKind::Winget.program()).arg("--version")
}

fn source_export_command() -> CommandSpec {
    CommandSpec::new(ManagerKind::Winget.program())
        .args(["source", "export"])
        .args(common_flags())
}

/// The sources winget currently has registered, or an empty list.
///
/// Separate from `diagnose` because selecting a source needs only this one
/// fact, and because an empty list is the correct answer for every failure
/// mode here: without a readable registration list osdk must omit `--source`
/// rather than name a source winget may not have.
pub fn registered_sources(runner: &dyn CommandRunner, limits: CaptureLimits) -> Vec<SourceRecord> {
    let outcome = runner.run_captured(&source_export_command(), limits);
    let (probe, _) = outcome_of(&outcome);
    if probe != ProbeOutcome::Succeeded {
        return Vec::new();
    }
    stdout_text(&outcome)
        .as_deref()
        .map(parse_exported_sources)
        .unwrap_or_default()
}

/// Every package winget reports as installed, with versions where it has them.
///
/// Uses `winget export --include-versions`, whose schema 2.0 JSON carries
/// English keys regardless of display language. The `list` table would be
/// simpler to call and impossible to parse safely: its headers are localized, so
/// an English-header parser silently finds no column and concludes nothing is
/// installed.
///
/// `None` means winget could not be queried, which callers must report as
/// unknown rather than as an empty host. Export writes to a file rather than
/// stdout, so a caller-supplied directory is used and the file removed.
///
/// Export also prints a warning to stderr for installed programs it cannot
/// trace back to any source, while still exiting 0. That is expected and does
/// not invalidate the packages it did resolve.
pub fn installed_packages(
    runner: &dyn CommandRunner,
    limits: CaptureLimits,
    directory: &std::path::Path,
) -> Option<Vec<super::status::ExportedPackage>> {
    let target = directory.join("osdk-winget-export.json");
    let outcome = runner.run_captured(&export_command(&target), limits);
    let (probe, _) = outcome_of(&outcome);
    if probe != ProbeOutcome::Succeeded {
        let _ = std::fs::remove_file(&target);
        return None;
    }
    let json = std::fs::read_to_string(&target).ok();
    let _ = std::fs::remove_file(&target);
    json.map(|json| super::status::parse_exported_packages(&json))
}

fn export_command(target: &std::path::Path) -> CommandSpec {
    CommandSpec::new(ManagerKind::Winget.program())
        .args(["export", "-o"])
        .arg(target)
        // Without this the export omits versions, and a pinned request could
        // never be evaluated.
        .arg("--include-versions")
        .args(common_flags())
}

/// Classify a completed probe without looking at its text.
fn outcome_of(command: &CommandOutcome) -> (ProbeOutcome, Option<i32>) {
    match command {
        CommandOutcome::NotInstalled => (ProbeOutcome::NotInstalled, None),
        CommandOutcome::PermissionDenied => (ProbeOutcome::PermissionDenied, None),
        CommandOutcome::SpawnFailed { .. } => (ProbeOutcome::SpawnFailed, None),
        CommandOutcome::TimedOut { .. } => (ProbeOutcome::TimedOut, None),
        CommandOutcome::ExecutionFailed { .. } => (ProbeOutcome::SpawnFailed, None),
        CommandOutcome::Exited { status, .. } => {
            let code = status.code();
            if status.success() {
                (ProbeOutcome::Succeeded, code)
            } else {
                (ProbeOutcome::Failed, code)
            }
        }
    }
}

fn stdout_text(command: &CommandOutcome) -> Option<String> {
    command
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Inspect winget on this host, read-only.
///
/// Runs at most two probes and never installs, configures, or elevates.
pub fn diagnose(runner: &dyn CommandRunner, limits: CaptureLimits) -> ManagerReport {
    if !cfg!(windows) {
        return ManagerReport::not_applicable(ManagerKind::Winget);
    }

    let version_outcome = runner.run_captured(&version_command(), limits);
    let (version_probe, version_code) = outcome_of(&version_outcome);

    let mut report = ManagerReport::new(ManagerKind::Winget, ManagerStatus::Healthy);
    report.record_probe(ProbeRecord {
        purpose: ProbePurpose::VersionQuery,
        outcome: version_probe,
        exit_code: version_code,
    });

    // Without a working client there is nothing further to ask, and the reason
    // it is unusable decides what doctor should tell the user to do.
    let status = match version_probe {
        ProbeOutcome::Succeeded => None,
        ProbeOutcome::NotInstalled => Some(ManagerStatus::NotInstalled),
        ProbeOutcome::PermissionDenied => Some(ManagerStatus::PermissionDenied),
        ProbeOutcome::TimedOut => Some(ManagerStatus::Unresponsive),
        ProbeOutcome::Failed | ProbeOutcome::SpawnFailed => Some(ManagerStatus::Degraded),
    };

    if let Some(status) = status {
        let mut unusable = ManagerReport::new(ManagerKind::Winget, status);
        unusable.record_probe(ProbeRecord {
            purpose: ProbePurpose::VersionQuery,
            outcome: version_probe,
            exit_code: version_code,
        });
        for capability in [
            Capability::Client,
            Capability::VersionQuery,
            Capability::SourceEnumeration,
            Capability::PackageEnumeration,
            Capability::MirrorInspection,
        ] {
            unusable.set_capability(capability, CapabilityStatus::Unavailable);
        }
        return unusable;
    }

    report.set_capability(Capability::Client, CapabilityStatus::Supported);
    report.set_capability(Capability::VersionQuery, CapabilityStatus::Supported);

    let mut details = WingetDetails {
        version: stdout_text(&version_outcome)
            .as_deref()
            .and_then(parse_version),
        ..WingetDetails::default()
    };

    let source_outcome = runner.run_captured(&source_export_command(), limits);
    let (source_probe, source_code) = outcome_of(&source_outcome);
    report.record_probe(ProbeRecord {
        purpose: ProbePurpose::SourceEnumeration,
        outcome: source_probe,
        exit_code: source_code,
    });

    if source_probe == ProbeOutcome::Succeeded {
        details.sources = stdout_text(&source_outcome)
            .as_deref()
            .map(parse_exported_sources)
            .unwrap_or_default();
        details.has_non_default_source = has_non_default_source(&details.sources);
        report.set_capability(Capability::SourceEnumeration, CapabilityStatus::Supported);
        report.set_capability(Capability::MirrorInspection, CapabilityStatus::Supported);
    } else {
        // The client works, so osdk can still do some things; say so rather
        // than declaring the whole manager unusable.
        report.status = ManagerStatus::Degraded;
        report.set_capability(Capability::SourceEnumeration, CapabilityStatus::Unavailable);
        report.set_capability(Capability::MirrorInspection, CapabilityStatus::Unavailable);
    }

    // Enumerating installed packages needs `winget export`, which writes to a
    // file rather than stdout and is therefore left to a later phase.
    report.set_capability(Capability::PackageEnumeration, CapabilityStatus::Unknown);
    report.set_details(ManagerDetails::Winget(details));
    report
}

/// The probes `diagnose` would run, for tests and for documentation.
pub fn probe_purposes() -> BTreeSet<ProbePurpose> {
    BTreeSet::from([ProbePurpose::VersionQuery, ProbePurpose::SourceEnumeration])
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::ExitStatus;
    use std::sync::Mutex;

    use super::*;
    use crate::process::CapturedOutput;

    /// Captured verbatim from `winget source export` on Windows 11 with
    /// winget v1.29.290, display language Chinese. The keys are English even
    /// there, which is the property this module depends on.
    const REAL_SOURCE_EXPORT: &str = concat!(
        r#"{"Arg":"https://storeedgefd.dsx.mp.microsoft.com/v9.0","Data":"","Explicit":false,"Identifier":"StoreEdgeFD","Name":"msstore","TrustLevel":["Trusted"],"Type":"Microsoft.Rest"}"#,
        "\n",
        r#"{"Arg":"https://cdn.winget.microsoft.com/cache","Data":"Microsoft.Winget.Source_8wekyb3d8bbwe","Explicit":false,"Identifier":"Microsoft.Winget.Source_8wekyb3d8bbwe","Name":"winget","TrustLevel":["Trusted","StoreOrigin"],"Type":"Microsoft.PreIndexed.Package"}"#,
        "\n",
        r#"{"Arg":"https://cdn.winget.microsoft.com/fonts","Data":"Microsoft.Winget.Fonts.Source_8wekyb3d8bbwe","Explicit":true,"Identifier":"Microsoft.Winget.Fonts.Source_8wekyb3d8bbwe","Name":"winget-font","TrustLevel":["Trusted","StoreOrigin"],"Type":"Microsoft.PreIndexed.Package"}"#,
        "\n",
    );

    struct ScriptedRunner {
        responses: Mutex<Vec<CommandOutcome>>,
        programs: Mutex<Vec<String>>,
        arguments: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedRunner {
        fn new(responses: Vec<CommandOutcome>) -> Self {
            Self {
                responses: Mutex::new(responses),
                programs: Mutex::new(Vec::new()),
                arguments: Mutex::new(Vec::new()),
            }
        }

        fn recorded_arguments(&self) -> Vec<Vec<String>> {
            self.arguments.lock().unwrap().clone()
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run_captured(&self, command: &CommandSpec, _limits: CaptureLimits) -> CommandOutcome {
            self.programs
                .lock()
                .unwrap()
                .push(command.program().to_string_lossy().into_owned());
            self.arguments.lock().unwrap().push(
                command
                    .arguments()
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
            );
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                CommandOutcome::NotInstalled
            } else {
                responses.remove(0)
            }
        }

        fn run_foreground(&self, _command: &CommandSpec) -> io::Result<ExitStatus> {
            panic!("discovery must never run a foreground command");
        }
    }

    fn exited(code: i32, stdout: &str) -> CommandOutcome {
        CommandOutcome::Exited {
            status: synthetic_status(code),
            output: CapturedOutput {
                stdout: stdout.as_bytes().to_vec(),
                ..CapturedOutput::default()
            },
        }
    }

    #[cfg(windows)]
    fn synthetic_status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }

    #[cfg(unix)]
    fn synthetic_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn parses_the_json_lines_a_real_host_emitted() {
        let sources = parse_exported_sources(REAL_SOURCE_EXPORT);

        assert_eq!(sources.len(), 3);
        let winget = sources
            .iter()
            .find(|source| source.name == "winget")
            .expect("the default winget source");
        assert_eq!(
            winget.endpoint.as_deref(),
            Some("https://cdn.winget.microsoft.com/cache")
        );
        assert_eq!(winget.kind.as_deref(), Some("Microsoft.PreIndexed.Package"));
        assert_eq!(winget.trust, SourceTrust::Trusted);
    }

    #[test]
    fn a_stock_host_reports_no_mirror() {
        let sources = parse_exported_sources(REAL_SOURCE_EXPORT);
        assert!(
            !has_non_default_source(&sources),
            "a host with only built-in sources is not using a mirror"
        );
    }

    #[test]
    fn a_configured_mirror_is_detected() {
        let mirrored = format!(
            "{REAL_SOURCE_EXPORT}{}\n",
            r#"{"Arg":"https://mirrors.ustc.edu.cn/winget-source","Data":"","Explicit":false,"Identifier":"USTC.Mirror","Name":"ustc","TrustLevel":["Trusted"],"Type":"Microsoft.PreIndexed.Package"}"#
        );

        let sources = parse_exported_sources(&mirrored);
        assert_eq!(sources.len(), 4);
        assert!(has_non_default_source(&sources));
    }

    #[test]
    fn an_untrusted_source_is_not_reported_as_trusted() {
        let line = r#"{"Arg":"https://example.invalid/src","Identifier":"Example","Name":"example","TrustLevel":["None"],"Type":"Microsoft.PreIndexed.Package"}"#;

        let sources = parse_exported_sources(line);
        assert_eq!(sources[0].trust, SourceTrust::Untrusted);
    }

    #[test]
    fn a_source_without_a_trust_level_is_unknown_rather_than_trusted() {
        let line =
            r#"{"Arg":"https://example.invalid/src","Identifier":"Example","Name":"example"}"#;

        let sources = parse_exported_sources(line);
        assert_eq!(
            sources[0].trust,
            SourceTrust::Unknown,
            "absence of evidence must not be reported as trust"
        );
    }

    #[test]
    fn one_malformed_line_does_not_hide_the_others() {
        let mixed = format!("not json at all\n{REAL_SOURCE_EXPORT}");

        let sources = parse_exported_sources(&mixed);
        assert_eq!(sources.len(), 3, "the three valid sources must survive");
    }

    #[test]
    fn unknown_fields_from_a_future_winget_are_tolerated() {
        let line = r#"{"Arg":"https://example.invalid/s","Identifier":"E","Name":"e","TrustLevel":["Trusted"],"SomeFutureField":{"nested":1}}"#;

        let sources = parse_exported_sources(line);
        assert_eq!(sources.len(), 1, "a new key must not break discovery");
    }

    #[test]
    fn parses_the_version_a_real_host_printed() {
        assert_eq!(parse_version("v1.29.290\n").as_deref(), Some("v1.29.290"));
    }

    #[test]
    fn calls_never_pass_the_undocumented_no_progress_flag() {
        let runner = ScriptedRunner::new(vec![
            exited(0, "v1.29.290\n"),
            exited(0, REAL_SOURCE_EXPORT),
        ]);

        let _ = diagnose(&runner, CaptureLimits::default());

        for arguments in runner.recorded_arguments() {
            assert!(
                !arguments.iter().any(|argument| argument == "--no-progress"),
                "--no-progress is undocumented on 1.29.290 and changes nothing when redirected"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_healthy_host_reports_its_sources() {
        let runner = ScriptedRunner::new(vec![
            exited(0, "v1.29.290\n"),
            exited(0, REAL_SOURCE_EXPORT),
        ]);

        let report = diagnose(&runner, CaptureLimits::default());

        assert_eq!(report.status, ManagerStatus::Healthy);
        assert_eq!(
            report.capabilities.get(&Capability::SourceEnumeration),
            Some(&CapabilityStatus::Supported)
        );
        let Some(ManagerDetails::Winget(details)) = report.details else {
            panic!("winget details expected");
        };
        assert_eq!(details.version.as_deref(), Some("v1.29.290"));
        assert_eq!(details.sources.len(), 3);
    }

    #[cfg(windows)]
    #[test]
    fn an_absent_client_short_circuits_without_a_second_probe() {
        let runner = ScriptedRunner::new(vec![CommandOutcome::NotInstalled]);

        let report = diagnose(&runner, CaptureLimits::default());

        assert_eq!(report.status, ManagerStatus::NotInstalled);
        assert_eq!(
            runner.recorded_arguments().len(),
            1,
            "there is nothing to ask a client that is not there"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_working_client_with_unreadable_sources_is_degraded_not_dead() {
        let runner = ScriptedRunner::new(vec![
            exited(0, "v1.29.290\n"),
            exited(exit_code::SOURCE_NAME_DOES_NOT_EXIST, ""),
        ]);

        let report = diagnose(&runner, CaptureLimits::default());

        assert_eq!(report.status, ManagerStatus::Degraded);
        let recorded = report
            .probes
            .iter()
            .find(|probe| probe.purpose == ProbePurpose::SourceEnumeration)
            .expect("the source probe");
        assert_eq!(
            recorded.exit_code,
            Some(exit_code::SOURCE_NAME_DOES_NOT_EXIST),
            "the raw code is the locale-independent part and must be kept"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn winget_is_not_applicable_off_windows() {
        let runner = ScriptedRunner::new(Vec::new());

        let report = diagnose(&runner, CaptureLimits::default());

        assert_eq!(report.status, ManagerStatus::NotApplicable);
        assert!(
            runner.recorded_arguments().is_empty(),
            "no process should be spawned looking for winget on a non-Windows host"
        );
    }
}
